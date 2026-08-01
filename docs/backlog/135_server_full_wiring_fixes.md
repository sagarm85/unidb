# unidb-server-full wiring fixes (memory storage + ConnectInfo)

**Type:** Improvement
**Status:** SHIPPED (2026-08-01, PR #238)

> Two real, binary-specific bugs in **`unidb-server-full`** (the storage-capable
> server binary), reported by the unidb-studio session during its live
> verification and reconfirmed present as of PR #234. Both are a few lines each;
> the plain `unidb-server` binary was already correct, so this brings the full
> binary in line with it. No engine/storage-library change.

## Bug 1 — `STORAGE_BACKEND=memory` was unreachable
`try_init_storage` unconditionally called `S3ObjectStore::from_config(&cfg)`
regardless of the configured `Backend`, so `STORAGE_BACKEND=memory` — the
Docker-free `MemoryObjectStore` test double — still demanded S3 credentials and
could never activate from this binary.

**Fix:** select the store by `cfg.backend`: `Backend::Memory` →
`MemoryObjectStore::new(cfg.bucket)`; `Minio`/`S3` → `S3ObjectStore::from_config`
(unchanged path). `StorageConfig`/`Backend` already existed and parse
`STORAGE_BACKEND` correctly — only the binary ignored the result.

## Bug 2 — missing `ConnectInfo<SocketAddr>` wiring 500'd all auth
The binary served with `axum::serve(listener, router)` instead of
`router.into_make_service_with_connect_info::<SocketAddr>()`, so the item-121
auth rate limiter's `ConnectInfo<SocketAddr>` extractor had nothing to resolve
against and every rate-limited auth route (`POST /auth/login|signup|refresh`)
returned `500`. The plain `unidb-server` binary already wires this correctly
(that's what the session's live verification used instead).

**Fix:** serve with `into_make_service_with_connect_info::<std::net::SocketAddr>()`,
mirroring `src/bin/unidb-server.rs`.

## Incidental
Fixed a pre-existing `clippy::manual_ignore_case_cmp` in the `UNIDB_DEV_LOGIN`
parse (`v.to_ascii_lowercase() == "true"` → `v.eq_ignore_ascii_case("true")`,
matching the `UNIDB_ALLOW_SIGNUP` parse right below it) — never caught because
the main-crate `clippy --all-features --all-targets` gate does not cover this
separate workspace-member binary. **Follow-up flagged:** add `unidb-server-full`
(and the other workspace-member crates) to the clippy gate so binary-only lints
are caught in CI.

## Verification
- `cargo build -p unidb-server-full` clean; `cargo clippy -p unidb-server-full
  -- -D warnings` clean; `cargo fmt --check` clean; crash harness **54/54**.
- **Live smoke test of the actual binary** (no committed subprocess harness — not
  the repo's pattern; the fixes mirror the router-level wiring already covered by
  `server_rate_limit`/storage TestServer tests): started `unidb-server-full` with
  `STORAGE_BACKEND=memory` → log `"storage service ready","backend":"memory"`
  (Bug 1 fixed); `POST /auth/login` returned **422** (Json body validation), not
  **500** (Bug 2 fixed — the request passed the `ConnectInfo` extractor).

## Follow-up
- Add the workspace-member binaries to the clippy gate (see Incidental).
- A committed binary-spawn smoke test for `unidb-server-full` would guard both
  wirings against regression; deferred as it's not the current test pattern.
