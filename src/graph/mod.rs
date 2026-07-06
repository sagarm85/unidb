// Graph support (M3): edge records over the existing row/heap/catalog
// machinery — see `edges.rs`'s module doc for why no new storage-layer
// code was needed. `parser`/`logical`/`executor` (the Cypher subset) land
// in M3.c.

pub mod edges;
pub mod index;
