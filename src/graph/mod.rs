// Graph support (M3): edge records over the existing row/heap/catalog
// machinery — see `edges.rs`'s module doc for why no new storage-layer
// code was needed. `parser`/`logical`/`executor` (the Cypher subset) land
// in M3.c.

// Item 95: per-engine adjacency cache (hot-hub lazy warm cache).
pub mod adjacency_cache;
pub mod edges;
pub mod executor;
pub mod index;
pub mod logical;
pub mod parser;
