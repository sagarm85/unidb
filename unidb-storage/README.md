# unidb-storage — object storage service (backlog item 23)

A Supabase-Storage analog, built as an **app-layer** crate over unidb (like
`unidb-dispatch`). Bucket/object **metadata** is transactional in unidb tables;
object **bytes** are tiered:

- **small objects (`< inline_threshold`, default 1 MiB) → engine LOBs** — bytes +
  metadata commit/roll back in **one transaction** (the P3.d ACID-inline edge).
- **large objects → an S3-wire object store** (MinIO in dev, S3 in prod), with
  **presigned PUT/GET** so browsers move bytes directly — the engine never
  proxies a large payload (CLAUDE.md §10).

Consistency for the S3 tier rides an **outbox**: the metadata row + its
"upload-pending" event commit atomically (events are enabled on `objects`), and a
**reconciler** confirms uploads (`pending → ready`) or **compensates**
(`pending → failed` + dead-letter, never a dangling pending) and sweeps orphaned
bytes.

This crate adds **no engine surface** and keeps `tokio` + the AWS SDK out of the
`unidb` engine crate — the "engine stays sync" invariant holds. See the full
design note: [`docs/design/storage_service.md`](../docs/design/storage_service.md).

## Quick start (embedded)

```rust
use std::sync::Arc;
use unidb::Engine;
use unidb_storage::{StorageConfig, StorageService, MemoryObjectStore, Reconciler};

# async fn run() -> unidb_storage::Result<()> {
let engine = Arc::new(Engine::open(std::path::Path::new("./db"), 0)?);
let store = Arc::new(MemoryObjectStore::new("unidb")); // or S3ObjectStore::from_config(&cfg)?
let svc = StorageService::new(engine.clone(), store.clone(), StorageConfig::memory()).await?;

svc.create_bucket("photos", Some("alice")).await?;
svc.put_object("photos", "cat.png", std::fs::read("cat.png").unwrap(), Some("image/png"), Some("alice")).await?;
let bytes = svc.get_object("photos", "cat.png").await?;

// Run the reconciler periodically to confirm/compensate S3-tier uploads and
// sweep orphans:
let recon = Reconciler::new(engine, store, StorageConfig::memory());
let _report = recon.run_once().await?;
# Ok(()) }
```

For a large object the browser flow is: `begin_upload` → (browser PUTs to the
returned presigned URL) → `finish_upload` (or let the reconciler confirm).

## Configuration (env)

`StorageConfig::from_env()` reads:

| Var | Default | Meaning |
|---|---|---|
| `STORAGE_BACKEND` | `memory` | `memory` \| `minio` \| `s3` |
| `STORAGE_ENDPOINT` / `STORAGE_S3_ENDPOINT` | — | object-store endpoint (required for MinIO) |
| `STORAGE_REGION` | `us-east-1` | S3 region |
| `STORAGE_BUCKET` | `unidb` | the one physical store bucket (objects namespaced by `<bucket>/<key>`) |
| `STORAGE_ACCESS_KEY` / `STORAGE_SECRET_KEY` | — | static creds (required for minio/s3) |
| `STORAGE_FORCE_PATH_STYLE` | minio→true, s3→false | path-style vs virtual-host addressing |
| `STORAGE_INLINE_THRESHOLD` | `1048576` | bytes below which objects go inline as LOBs |
| `STORAGE_PRESIGN_TTL_SECS` | `900` | presigned URL lifetime |
| `STORAGE_PENDING_GRACE_SECS` | `300` | age after which a pending upload with no bytes is compensated |
| `STORAGE_ORPHAN_GRACE_SECS` | `3600` | age after which unreferenced store bytes are swept |

`minio` and `s3` are **one S3-wire impl** (`S3ObjectStore`), differing only in
config. Credentials come from env as static keys (the documented "creds via env"
contract; the IAM provider chain is deliberately not pulled in — it would add a
heavy async dependency).

## Testing

`cargo test -p unidb-storage` passes **without Docker**: the default tests use the
in-process `MemoryObjectStore`, and presigned-URL generation is unit-tested
**offline** against `S3ObjectStore` (SigV4 signing is local, no network).

The **one** live test, `live_store_round_trip_when_configured`, is **gated behind
`STORAGE_S3_ENDPOINT`** — it returns early (skips) unless that env var is set. To
run it against a real MinIO:

```sh
docker compose -f docker/docker-compose.minio.yml up -d   # brings up MinIO + creates the `unidb` bucket
export STORAGE_S3_ENDPOINT=http://localhost:9000
export STORAGE_ACCESS_KEY=minioadmin STORAGE_SECRET_KEY=minioadmin STORAGE_BUCKET=unidb
cargo test -p unidb-storage live_store_round_trip_when_configured -- --nocapture
```

## Out of scope

The studio **"Storage" tab** (bucket browser, upload UI, presigned-link copy)
lives in the `unidb-studio` repo — like the Events/Logs tabs before it.
