# Object storage service — MinIO (dev) / S3 (prod) over engine metadata

**Type:** Milestone
**Status:** SHIPPED (2026-07-13, branch `23-storage-service`, PR pending —
STOP-for-review, do not merge). Implemented as the new app-layer `unidb-storage`
crate; design note `docs/design/storage_service.md`; metrics/evidence in
`PROGRESS.md` ("Object storage service (item 23)").

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

- [x] Upload/download/delete round-trip on both backends via one config switch;
      docker-compose brings up MinIO for dev. — `tests/round_trip.rs`
      (memory/inline + s3-tier); the same `S3ObjectStore` serves MinIO & S3 by
      config; `docker/docker-compose.minio.yml` + gated
      `live_store_round_trip_when_configured`.
- [x] Kill the service mid-upload: no metadata row without bytes (outbox
      compensation) and no unreferenced bytes surviving the reconciler. —
      `tests/crash_consistency.rs` proves **both** directions deterministically
      (compensate pending→failed+DLQ; orphan-byte sweep) + `tests/scale.rs`.
- [x] Sub-threshold object round-trips as a LOB inside a user transaction
      (commit/rollback proof). — `tests/round_trip.rs`
      `inline_write_rolls_back_leaving_no_object_and_no_bytes` (abort leaves no
      row **and** no readable LOB; commit persists).
- [ ] Studio "Storage" tab: bucket browser, upload, presigned-link copy. —
      **out of this repo** (`unidb-studio`), by design; noted, not built.

## Depends on

- Item 20 (dispatcher/outbox). Engine changes: **none** — proven. The one
  engine constraint hit (single-page catalog blob ceiling) was worked *around*
  in the service layer (compact schema, all DDL up front), not by changing the
  engine. See the dated correction in `docs/design/storage_service.md` §4.
