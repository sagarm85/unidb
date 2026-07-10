//! Isolated home for the loom model of the `DiskBTree` crabbing latch protocol
//! (index-write-concurrency, Validation §3). The model itself lives in
//! `tests/crabbing.rs`; this lib is intentionally empty — the crate exists only
//! to contain the `--cfg loom` build so it does not spread to `unidb`'s other
//! dev-dependencies. See `Cargo.toml` for the rationale and run command.
