# Object storage service (`unidb-storage`) — design note

**Backlog item:** [`23_storage_service.md`](../backlog/23_storage_service.md)
**Depends on:** item 20 (dispatcher/outbox — `unidb-dispatch`), P3.d LOBs.
**Status:** committed first (2026-07-13), ahead of implementation, per the item-23
task brief ("justify the crate choice … in a short design note committed first").

This note records the two decisions the brief calls "landmines to decide up
front": the **S3 client crate**, and **how the outbox uploader/reconciler is
driven** (including the compensation path and the one place the Dispatcher's
built-in retry does *not* fit — surfaced honestly here rather than papered over).

---

## 1. Boundary (why this is a service, not engine code)

CLAUDE.md §10 forbids "S3 tiering **in** the engine". So `unidb-storage` is a new
**app-layer workspace crate**, exactly like `unidb-dispatch`:

- It embeds `Arc<Engine>` and uses only the **already-shipped** public engine
  surface (`execute_sql[_params]`, `begin`/`commit`/`abort`,
  `put_large_object`/`read_large_object`/`delete_large_object`,
  `enable_events`/`poll_events`/`ack_events`). It adds **no** engine method and
  **no** new engine REST route.
- `tokio` + the AWS SDK live in *this* crate only. The engine's default build
  stays sync (`cargo tree -p unidb --no-default-features --edges normal` shows no
  async runtime — unchanged by this crate).

Bucket/object **metadata** lives in ordinary unidb tables (`buckets`,
`objects`). Object **bytes** live either inline in the engine (small objects, as
P3.d LOBs — ACID) or in an S3-wire object store (large objects). The engine is
the source of truth for metadata; the store holds opaque bytes it never has to
reason about transactionally.

---

## 2. Decision A — S3 client crate: **`aws-sdk-s3`** (one wire impl, two endpoints)

Candidates the brief named: `object_store` vs `aws-sdk-s3` vs `rusoto`.

| Crate | Presign PUT/GET | MinIO (path-style, custom endpoint) | Maintained | Verdict |
|---|---|---|---|---|
| `rusoto` | partial | yes | **no** (archived) | reject — dead |
| `object_store` | limited/newer `Signer`; abstracts away the S3 specifics (path-style, endpoint) we must control | via `AmazonS3Builder` | yes | reject — presign is the hard requirement and its abstraction hides what we need |
| **`aws-sdk-s3`** | **first-class** — `PutObject/GetObject::presigned(PresigningConfig)` produces a SigV4-signed URL **with no network call** | `endpoint_url(..)` + `force_path_style(true)` + static creds | yes (official) | **chosen** |

**Presigning is the deciding factor.** The AC requires presigned PUT/GET so
browsers move bytes directly against the store; the engine must never proxy large
payloads (§10). In `aws-sdk-s3`, presigning is pure local SigV4 signing — it needs
credentials + endpoint config but **no live server**, so we can even unit-test URL
generation offline (see §6). `object_store`'s signing story is newer and its whole
value proposition — hiding the backend — works against us here, because MinIO
requires **path-style addressing** and a **custom endpoint**, which we must set
explicitly.

**"One impl, two endpoints" — yes, the SDK allows it.** MinIO speaks the S3 wire
protocol, so there is exactly **one** `ObjectStore` S3 implementation
(`store::s3::S3ObjectStore`). `STORAGE_BACKEND=minio|s3` does **not** select two
different code paths — it selects **config defaults**:

- `minio` → custom `endpoint_url` (e.g. `http://localhost:9000`), `force_path_style
  = true`, static creds from env (`STORAGE_ACCESS_KEY`/`STORAGE_SECRET_KEY`).
- `s3` → AWS regional endpoint, virtual-host style, creds from the standard AWS
  provider chain (or the same static env creds).

We *keep* the `ObjectStore` **trait** (not just the concrete struct) for one
reason: a **`store::memory::MemoryObjectStore`** in-process impl lets the whole
service + reconciler be tested deterministically **without Docker** (see §6 and
the crate README). The trait is the seam; `S3ObjectStore` is the production body;
`MemoryObjectStore` is the test body. This is the honest reading of the brief's
"two impls" — the two *backends that matter operationally* (minio, s3) are one
wire impl behind config; the second trait impl is the Docker-free test double.

Added dependencies (app-layer only): `aws-sdk-s3`, `aws-config`,
`aws-credential-types`, `aws-smithy-runtime-api`/`aws-smithy-types` (transitive),
`tokio`, `async-trait`.

---

## 3. Data model (metadata as ordinary unidb tables)

```
buckets(name TEXT, created_by TEXT, created_at TIMESTAMP)
objects(
  bucket        TEXT,       -- FK-by-convention to buckets.name
  key           TEXT,       -- object key within the bucket
  size          INT,        -- bytes (known at confirm for S3 tier)
  etag          TEXT,       -- store etag / content hash (NULL until ready)
  content_type  TEXT,
  tier          TEXT,       -- 'inline' (LOB) | 's3'
  status        TEXT,       -- 'ready' | 'pending' | 'failed'
  lob_id        INT,        -- engine LOB id when tier='inline', else NULL
  storage_key   TEXT,       -- physical key in the object store when tier='s3'
  created_by    TEXT,
  created_at    TIMESTAMP
)
```

`(bucket, key)` is the logical identity. `enable_events("objects")` is called at
init so **every** insert/update/delete on `objects` emits an event atomically in
the writer's transaction — that event stream **is the outbox** (§4).

---

## 4. Decision B — tiering + outbox + reconciler (the consistency core)

> **Correction (2026-07-13, during implementation — evidence-based, per §9).**
> Two things in this note's original sketch changed on contact with a real
> engine constraint: **unidb persists the entire catalog (every `TableDef`) as
> one ~8 KiB page blob.** Measurements while building this crate:
> - The `objects` schema above with a `storage_key TEXT` column **plus** the
>   8-column `unidb_dispatch::dlq` table **overflows** that blob
>   (`HeapFull { size: 8883 }`). So (a) `storage_key` was **dropped** — it is
>   always `"<bucket>/<object_key>"`, derived not stored (`service::storage_key`);
>   and (b) compensation uses a **compact storage-native 4-column DLQ**
>   (`object_dlq`) instead of reusing `dlq::ensure_dlq_table`/`insert_dead_letter`
>   verbatim. The item-20 reuse that remains is real and load-bearing: the
>   optional `ConfirmSink` rides a genuine `unidb_dispatch::Dispatcher` +
>   `Filter` + `Sink`.
> - The catalog blob is only rewritten on a *catalog* mutation (DDL /
>   `enable_events`), and its in-memory size grows with row volume; a **runtime**
>   `CREATE TABLE` re-serializes the grown catalog and can overflow. So **all**
>   DDL (buckets, objects, object_dlq) + `enable_events` happens **up front in
>   `StorageService::new`**, before any data — the reconciler does **zero** DDL.
>   Verified: 1 000-object reconcile pass + reopen with no overflow
>   (`tests/scale.rs`). `created_at` is stored as `created_at_ms INT` (epoch
>   millis) so the reconciler can age pending rows directly.
> The rest of §4 reads as originally written; treat the four bullets above as the
> authoritative schema/DLQ details.

### 4.1 Two write paths, chosen by size threshold (`STORAGE_INLINE_THRESHOLD`, default 1 MiB)

**Small object (`size < threshold`) → engine LOB, fully ACID, no outbox needed.**
One user transaction:

```
xid = begin()
lob_id = put_large_object(xid, bytes)            -- chunked, streamed, in-txn
INSERT objects(..., tier='inline', status='ready', lob_id=lob_id)   -- same xid
commit(xid)     -- or abort(xid): leaves NO object row AND NO LOB bytes
```

Bytes and metadata commit or roll back together — this is the engine's P3.d edge
that Supabase Storage lacks, and it directly satisfies the AC "sub-threshold
object round-trips as a LOB inside a user transaction (commit **and** rollback
proof — rollback leaves no object)".

**Large object (`size ≥ threshold`) → S3/MinIO via presigned PUT, client-direct.**
Bytes and metadata **cannot** share a transaction (the store is a separate
system), so we use the **outbox** pattern:

1. `begin_upload`: one txn inserts `objects(tier='s3', status='pending',
   etag=NULL, storage_key=…, created_at=now)` and commits. Because events are
   enabled, this **atomically** writes an `objects/insert` event = the durable
   "upload-pending" outbox record. Return a **presigned PUT URL**.
2. The client uploads bytes **directly** to the store via that URL. The engine
   never sees the bytes (§10 satisfied).
3. **Confirm** flips `pending → ready` once the bytes are verified present in the
   store (HEAD). Two ways to reach confirm, belt-and-suspenders:
   - **Fast path:** the client calls `finish_upload` (or a Dispatcher
     `ConfirmSink`, see §4.2) right after uploading.
   - **Safety net:** the **reconciler** (§4.3) sweeps and confirms any pending row
     whose bytes have appeared, and **compensates** any pending row whose bytes
     never appeared.

### 4.2 Reusing `unidb-dispatch` — what genuinely composes, and the one wall

The brief says *prefer reusing the Dispatcher so retry/DLQ come for free*, and
flags as a landmine "**a Dispatcher Sink that performs the upload+confirm vs. a
dedicated worker**". We use **both**, each for what it is actually good at, and we
record the wall:

**The wall (surfaced honestly, per the task's honesty bar).** The Dispatcher's
`RetryPolicy` is a **tight, in-cycle** retry (default 3 attempts, tens of
milliseconds of exponential backoff) designed for a *transient* webhook flap. An
S3 upload grace window is **wall-clock seconds-to-minutes** (the client is still
uploading). Driving confirm purely off the Sink's Err→retry→dead-letter path
would either (a) dead-letter a perfectly good upload after ~150 ms because the
bytes have not landed yet, or (b) force the whole sequential dispatch pipeline to
sleep for minutes per pending event. **The correct "is this upload late?" signal
is the pending row's `created_at` age, not a retry counter** — which is a
reconciler-sweep concern, not a Sink concern.

**Resolution (no AC change needed — the AC says "an uploader/reconciler confirms
or compensates", and we build the reconciler):**

- **Confirm/compensate authority = a dedicated `Reconciler`** (§4.3), keyed on
  `created_at` age. This is the mechanism the AC's crash tests exercise.
- **Genuine Dispatcher reuse, two ways:**
  1. **DLQ machinery reused verbatim.** Compensation dead-letters through
     `unidb_dispatch::dlq::{ensure_dlq_table, insert_dead_letter}` — the exact
     dogfooded dead-letter table from item 20. "retry/DLQ come for free" is
     literally true: we call item-20 code, just from the reconciler instead of
     from a Sink's exhaustion path.
  2. **Optional `ConfirmSink` fast path.** A real `unidb_dispatch::Dispatcher`
     subscribed to `Filter::table("objects").ops(["insert"])` with a
     `ConfirmSink` gives event-driven fast confirmation + fan-out for free. Its
     retry is bounded and it is **not** the thing that flips a row to `failed` —
     if it cannot confirm within its short window it simply stops (the reconciler
     is the authority). This proves the item-20 outbox contract end-to-end (the
     stated point of item 23: "the proof the Milestone-18 contract is sufficient
     for a real service") without misusing the retry loop as a grace timer.

This division is the substantive design decision of item 23. It is recorded here
(dated) as the brief's honesty bar requires; it is **not** a proxy-through-the-
engine shortcut and it violates no §10 rule.

### 4.3 The reconciler (guarantees both AC crash directions)

`Reconciler::run_once` (run on an interval, or once in tests) does three sweeps:

1. **Confirm pending → ready.** For each `objects` row `status='pending'`: HEAD
   the store key. Present ⇒ `UPDATE … status='ready', etag=…, size=…`.
2. **Compensate stale pending → failed + DLQ.** Pending row absent from the store
   **and** `age(created_at) > pending_grace` ⇒ `UPDATE … status='failed'` **and**
   write a dead-letter row (reusing item-20 DLQ helpers). **Never a dangling
   pending** → satisfies "no metadata row without bytes (outbox compensation)".
3. **Orphan byte sweep.** `list` the store; any key with **no** non-failed
   `objects` row referencing it **and** older than `orphan_grace` ⇒ `delete` it.
   This reclaims bytes whose metadata transaction rolled back or was compensated
   → satisfies "no unreferenced bytes surviving the reconciler".

Grace windows are config (`pending_grace`, `orphan_grace`); tests set them to
zero for determinism.

### 4.4 Crash-consistency mapping (AC "kill mid-upload", both directions)

- **Direction 1 — pending metadata, bytes never arrived** (killed before/at the
  client PUT): the pending row exists, the store has nothing. Sweep 2 marks it
  `failed` + dead-letters it. There is **never** a `ready` row without bytes. Test
  proves it with `MemoryObjectStore` holding no bytes and `pending_grace=0`.
- **Direction 2 — bytes uploaded, metadata never committed** (client PUT
  succeeded but the metadata txn rolled back / was aborted): the store has a key
  with no live metadata. Sweep 3 deletes it. Test proves it by seeding
  `MemoryObjectStore` with an unreferenced key and `orphan_grace=0`.

Both are deterministic and Docker-free.

---

## 5. Presigned URLs

`ObjectStore::presign_put(key, ttl)` / `presign_get(key, ttl)` return a URL a
browser uses directly. On `S3ObjectStore` these are SigV4-signed (local, no
network). On `MemoryObjectStore` they return an opaque in-process stub URL (there
is no HTTP server) — the test double proves the *service wiring* returns a URL;
the *signing* itself is unit-tested offline against `S3ObjectStore` (§6).

---

## 6. Testing without Docker (the gate)

`cargo test --workspace` must pass with **no** Docker/MinIO running. Therefore:

- **Default tests use `MemoryObjectStore`** — full round-trip, both crash
  directions, LOB commit/rollback, all deterministic and in-process.
- **Presign generation is unit-tested offline** against `S3ObjectStore` built
  with fake static creds + a fake endpoint: assert the returned URL is
  `https?://…` and carries SigV4 query params (`X-Amz-Signature`, …). No live
  server needed — proves the "presigned URL on the MinIO path" landmine is real,
  not hand-waved.
- **Live MinIO round-trip is gated behind `STORAGE_S3_ENDPOINT`.** If that env var
  is unset the test returns early (skips). `docker/docker-compose.yml` gains a
  `minio` service so a developer can set the var and run it. The gate mechanism is
  documented in `unidb-storage/README.md`.

---

## 7. Out of scope (noted, not built)

- The studio **"Storage" tab** (bucket browser, upload UI, presigned-link copy)
  lives in the `unidb-studio` repo, like the Events/Logs tabs before it.
- Multipart upload, lifecycle policies, per-object RLS (item 24), CDN signing.
