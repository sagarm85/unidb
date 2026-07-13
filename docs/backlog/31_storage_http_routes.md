**Type:** Improvement
**Status:** ✅ SHIPPED 2026-07-14 — see `PROGRESS.md` item 31

---

# Item 31 — Storage HTTP routes

Surface the `unidb-storage` app-layer crate (item 23) as 7 protected REST
endpoints under `/storage/*` on the `unidb-server`, with a `StorageApi` trait
abstraction that avoids a circular crate dependency.

## Scope

- **Phase A** (Metadata gaps in `unidb-storage`): `list_buckets`, `list_objects`
  with prefix/delimiter virtual-folder filtering, `delete_bucket` with 409 guard
  (`BucketNotEmpty`).
- **Phase B** (AppState bootstrap): `storage: Option<Arc<dyn StorageApi>>` in
  `AppState`. Because `unidb-storage` already depends on `unidb`, adding
  `unidb-storage` to `unidb`'s `[dependencies]` would create a crate cycle.
  Solution: define `StorageApi` trait + value types at `unidb` crate root
  (`src/storage_api.rs`, no feature gate); `unidb-storage` implements it in
  `api_impl.rs`; `unidb-storage` in `[dev-dependencies]` only (available to
  integration tests, not the library). The binary defaults to
  `storage = None` (all `/storage/*` return 503); a custom embedding binary
  that depends on both crates can call `.with_storage(Some(Arc::new(svc)))`.
- **Phase C** (7 HTTP handlers): `GET/POST /storage/buckets`,
  `DELETE /storage/buckets/{name}`, `GET /storage/{bucket}/objects`,
  `PUT/DELETE /storage/{bucket}/objects/{*key}`,
  `GET /storage/{bucket}/presign/{*key}`. All in the JWT-protected sub-router.
  503-contract: every handler calls `require_storage` first.
- **Phase D** (Integration tests, `tests/storage_routes.rs`): 5 tests —
  unconfigured→503 for all routes; bucket CRUD + 409 on non-empty delete;
  inline object round-trip; presigned-ticket shape for large objects
  (threshold=10 bytes); virtual-folder listing with prefix+delimiter.

## Key design decisions

- `StorageApi` trait with `BoxFuture<'a, T>` return types for dyn-object
  compatibility (no `async_trait` crate needed).
- 503 STORAGE_NOT_AVAILABLE (not 500, not panic) when storage unconfigured.
- Inline vs presign split at `svc.inline_threshold()` (not Content-Length
  header — reads actual body bytes for correctness with memory-backed tests).
- Two-transaction delete_bucket (TOCTOU accepted per S3 semantics).
- Virtual-folder listing: pure Rust post-processing of `list_objects_in_bucket`
  results, no SQL changes.

## Gates passed

- `cargo test -p unidb --features server --test storage_routes` — 5/5 pass
- `cargo test --workspace --features server` — zero failures
- `cargo test --test crash` — 35/35 (unchanged)
- `cargo clippy --workspace --all-targets --features server -- -D warnings` — clean
- `cargo fmt --all -- --check` — clean
- `cargo tree -p unidb --no-default-features --edges normal | grep -i tokio` — empty (sync invariant intact)
- `cargo build` (no features) — clean
