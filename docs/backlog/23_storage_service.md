# Object storage service — MinIO (dev) / S3 (prod) over engine metadata

**Type:** Milestone
**Status:** IN PROGRESS (started 2026-07-13, branch `23-storage-service`) — design
note in `docs/design/storage_service.md`; implementation in the new
`unidb-storage` crate.

> Supabase-Storage analog, honoring the Milestone-18 boundary and §10 ("no S3
> tiering in the engine"): a **separate service layer** (`unidb-storage` crate
> or studio-backend module) that keeps bucket/object *metadata* transactional
> in unidb and object *bytes* in an object store. The engine already gives us
> one thing Supabase lacks: P3.d LOBs — chunked, streamed, fully transactional
> blobs — so small objects can be ACID-inline.

## Architecture

- **Metadata in unidb tables:** `buckets`, `objects` (key, size, etag,
  content-type, tier, created_by, …) — ordinary SQL, RLS-able later (item 24).
- **Bytes behind an `ObjectStore` trait:** `minio` (dev; add a `minio` service
  to `docker/compose`) and `s3` (prod) impls, selected by config
  (`STORAGE_BACKEND=minio|s3`, endpoint/creds via env). Same S3 wire API for
  both — one impl, two endpoints, if the SDK allows.
- **Hybrid tiering:** objects under a threshold (~1 MiB, config) stored as
  engine LOBs (transactional, crash-consistent); larger objects to S3/MinIO.
- **Consistency via outbox:** metadata row + "upload-pending" event commit
  atomically (M4 event queue, item 20 dispatcher); the uploader confirms or
  compensates — no orphaned metadata, orphaned bytes are GC-swept by a
  reconciler comparing store listings to metadata.
- **Presigned URLs:** service issues presigned PUT/GET so browsers move bytes
  directly against MinIO/S3; the engine never proxies large payloads.

## Acceptance

- [ ] Upload/download/delete round-trip on both backends via one config switch;
      docker-compose brings up MinIO for dev.
- [ ] Kill the service mid-upload: no metadata row without bytes (outbox
      compensation) and no unreferenced bytes surviving the reconciler.
- [ ] Sub-threshold object round-trips as a LOB inside a user transaction
      (commit/rollback proof).
- [ ] Studio "Storage" tab: bucket browser, upload, presigned-link copy.

## Depends on

- Item 20 (dispatcher/outbox). Engine changes: none expected — this is the
  proof the Milestone-18 contract is sufficient for a real service.
