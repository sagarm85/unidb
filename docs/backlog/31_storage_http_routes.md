# Storage HTTP route layer — wire `unidb-storage` into the engine HTTP server

**Type:** Milestone
**Status:** NOT STARTED

> The `unidb-storage` crate (item 23) is fully shipped as a library: it has
> `StorageService`, the outbox/reconciler, presigned-URL flows, and all
> metadata helpers. What is **not wired** is the engine HTTP server
> (`src/server/`). No `/storage/*` routes exist in `router.rs`, and
> `AppState` carries no `StorageService`. This milestone adds the HTTP surface
> that `unidb-studio`'s Storage tab (and any other S3-wire client) needs.

## Scope

Wire the existing `unidb-storage` crate into the server in three phases:

### Phase A — metadata API gaps in the crate (2 SP)

The HTTP handlers need three operations that `metadata.rs` and `StorageService`
do not yet expose:

| Missing method | Where to add | SQL |
|---|---|---|
| `list_buckets()` | `metadata.rs` + `StorageService` | `SELECT name FROM buckets ORDER BY name` |
| `list_objects(bucket, prefix, delimiter)` | `metadata.rs` + `StorageService` | `SELECT … FROM objects WHERE bucket=$1 AND object_key LIKE $prefix% AND status='ready'` then fold by `delimiter` (virtual-folder grouping) |
| `delete_bucket(name)` | `metadata.rs` + `StorageService` | `DELETE FROM buckets WHERE name=$1` (guard: bucket must be empty — error if any `objects` row exists for it) |

`list_objects` must return two slices: `prefixes: Vec<String>` (common-prefix
"folders") and `objects: Vec<ObjectRow>` — the same structure MinIO/S3 use for
virtual-directory listings, so the studio breadcrumb nav works.

### Phase B — `AppState` bootstrap (2 SP)

Add an optional `Arc<StorageService>` to `AppState`:

```rust
pub struct AppState {
    pub engine:   Arc<EngineHandle>,
    pub sessions: Arc<TxnSessions>,
    pub cursors:  Arc<CursorStore>,
    pub log_dir:  Arc<PathBuf>,
    // None when STORAGE_BACKEND env var is absent or "disabled"
    pub storage:  Option<Arc<StorageService>>,
}
```

- `AppState::new` / `AppState::with_config` call
  `StorageConfig::from_env()` and if config is present call
  `StorageService::new(engine.clone(), config).await` (spawn on the blocking
  pool — same pattern as `unidb-dispatch`). Log a warning and leave `None` if
  the env vars are missing or the MinIO endpoint is unreachable.
- Add `with_storage(svc: Arc<StorageService>)` builder for tests.
- Add `unidb-storage` as a dependency of the main `unidb` crate in
  `Cargo.toml` (feature-gated behind `storage` if binary size is a concern;
  otherwise unconditional — simpler).

### Phase C — 7 HTTP handlers + router wiring (10 SP)

All routes live under `/storage/*` in the `protected` sub-router (JWT auth
layer applies). Each handler extracts `state.storage` and returns
`HTTP 503 {"error":"storage not available"}` when `None` — same graceful-
degradation pattern the studio already handles.

| # | Method | Path | Handler | SP |
|---|--------|------|---------|-----|
| C1 | GET | `/storage/buckets` | `get_storage_buckets` — call `svc.list_buckets()`, return `{"buckets":[{"name":…}]}` | 1 |
| C2 | POST | `/storage/buckets` | `post_storage_bucket` — JSON `{"name":…,"public":bool}`, call `svc.create_bucket(name, …)`, 201 | 2 |
| C3 | DELETE | `/storage/buckets/:bucket` | `delete_storage_bucket` — call `svc.delete_bucket(name)`, 204; 409 if non-empty | 2 |
| C4 | GET | `/storage/buckets/:bucket/objects` | `get_storage_objects` — query params `prefix` + `delimiter`, return `{"prefixes":[…],"objects":[…]}` | 2 |
| C5 | POST | `/storage/buckets/:bucket/objects/*key` | `post_storage_object` — multipart or raw body ≤ inline threshold → `svc.put_object`; else → `svc.begin_upload` → return ticket with `presigned_put_url` | 3 |
| C6 | DELETE | `/storage/buckets/:bucket/objects/*key` | `delete_storage_object` — call `svc.delete_object`, 204 | 1 |
| C7 | GET | `/storage/buckets/:bucket/objects/*key/url` | `get_storage_object_url` — query param `expires` (secs, default 3600), call `svc.presign_get`, return `{"url":…,"expires_at_ms":…}` | 2 |

Router additions in `router.rs` (inside the existing `protected` block):

```rust
.route("/storage/buckets",           get(handlers::get_storage_buckets)
                                     .post(handlers::post_storage_bucket))
.route("/storage/buckets/:bucket",   delete(handlers::delete_storage_bucket))
.route("/storage/buckets/:bucket/objects",
                                     get(handlers::get_storage_objects))
.route("/storage/buckets/:bucket/objects/*key",
                                     post(handlers::post_storage_object)
                                     .delete(handlers::delete_storage_object))
.route("/storage/buckets/:bucket/objects/*key/url",
                                     get(handlers::get_storage_object_url))
```

### Phase D — integration tests (5 SP)

Add `tests/storage_routes.rs` (or extend `tests/integration/`):

- `test_storage_not_configured` — start server without env vars, assert all
  `/storage/*` routes return 503 with `"storage not available"`.
- `test_bucket_crud` — create → list (appears) → delete → list (gone);
  assert 409 on delete-non-empty.
- `test_object_round_trip_inline` — small object (< threshold): POST →
  GET /url → presigned GET round-trip → DELETE → gone.
- `test_object_round_trip_presigned` — large object: POST returns ticket,
  assert `presigned_put_url` is well-formed; skip actual MinIO upload
  (test env likely has no MinIO) with a `#[ignore]` annotation and a
  `MINIO_*` env gate.
- `test_list_objects_virtual_folders` — insert objects with `/`-delimited
  keys, assert `GET /objects?prefix=photos/&delimiter=/` returns correct
  `prefixes` + `objects` slices.

## Acceptance

- [ ] `GET /storage/buckets` returns the bucket list (200) or `{"supported":false}` (503) when unconfigured.
- [ ] Full create/list/delete bucket lifecycle via HTTP, 409 on non-empty delete.
- [ ] Small object (≤ 1 MiB) POST → immediate 201; object appears in list.
- [ ] Large object POST returns `{"presigned_put_url":…,"object_key":…}`; `finish_upload` can be called after the client PUT.
- [ ] `GET .../url` returns a time-bounded presigned GET URL.
- [ ] Virtual-folder listing (`prefix` + `delimiter`) returns correct `prefixes` / `objects` slices.
- [ ] All routes return 503 + `"storage not available"` when `STORAGE_BACKEND` is absent/disabled — no panic, no 500.
- [ ] `unidb-studio` Storage tab works end-to-end against a dev MinIO instance (manual smoke test; not gated on CI).
- [ ] `tests/storage_routes.rs` passes in CI (MinIO tests skipped via `#[ignore]`).

## Depends on

- Item 23 (`unidb-storage` crate, SHIPPED 2026-07-13) — the whole library layer.
- Item 25 (`25_multipage_catalog.md`, SHIPPED 2026-07-13) — lifted the catalog ceiling that constrained item 23's schema.
- `unidb-studio` PR #8 (Storage tab) — the client consuming these routes.

## Effort estimate

| Phase | SP |
|-------|----|
| A — metadata API gaps | 2 |
| B — AppState bootstrap | 2 |
| C — 7 HTTP handlers + router | 10 |
| D — integration tests | 5 |
| **Total** | **19** |

## Implementation notes

- `StorageService::new` is `async` — call it inside `tokio::spawn` or at
  server startup (which already runs inside a tokio runtime). Do **not** call
  it from `AppState::with_config` if that function is still sync; either make
  the bootstrap async or spawn a one-shot task and block on the handle.
- The `*key` wildcard in axum captures slashes — use `Path<(String, String)>`
  for `(bucket, key)` on wildcard routes.
- Phase C5 (`post_storage_object`): choose the upload path based on
  `Content-Length` header vs `StorageConfig::inline_threshold`. If
  Content-Length is absent, buffer up to `inline_threshold + 1` bytes to
  decide; stream the rest to S3 via `begin_upload`.
- Phase C3 (`delete_storage_bucket`): `delete_bucket` must hold a write
  transaction that checks `objects` table emptiness and deletes the bucket
  row atomically (no TOCTOU). Implement the empty-check guard in `metadata.rs`
  as part of Phase A.
- The `protected` router already carries the JWT middleware — no auth changes
  needed; storage routes inherit it for free.
