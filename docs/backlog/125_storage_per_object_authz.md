# Storage per-object authorization (item 120, Workstream F1)

**Type:** Improvement
**Status:** SHIPPED (→ PROGRESS.md left untouched per this task's scope — see
the commit on branch `claude/permissions-security-supabase-comparison-ixho2w`
for the change set)

> Spun out of [`120_supabase_parity_roadmap.md`](120_supabase_parity_roadmap.md)'s
> Workstream F ("Storage authorization"). F2 (signed URLs) and F3 (bucket
> public/private) are named in that roadmap as prerequisites that "already
> ship" — see the **correction** below; F1 (per-object policies) is this file.

## Correction to the roadmap doc (evidence-based, 2026-07-31)

The roadmap doc (`120_…md`) states, in its "Already implemented" section:
*"Storage authorization, partial — presigned GET/PUT URLs with TTL and
public/private buckets already ship (`unidb-storage`, item 23/31). Only
per-object RLS policies remain (see Workstream F)."*

**Presigned GET/PUT (F2) is accurate** — `StorageService::presign_get`/
`begin_upload` (`unidb-storage/src/service.rs`) return TTL-bound presigned
URLs via `aws-sdk-s3`, live since item 23/31.

**"public/private buckets already ship" (F3) was not true.** A Step-0 audit of
`unidb-storage/src/metadata.rs` (the `buckets`/`objects` table schemas),
`unidb-storage/src/service.rs`, and `src/server/storage.rs` (the seven
`/storage/*` HTTP routes) at the start of this task found:

- No `is_public` (or any visibility) column on the `buckets` table.
- No caller-identity check anywhere in the storage read/write/delete/list
  path — every `/storage/*` route ran the service call unauthenticated
  w.r.t. object ownership (the JWT was verified for *authentication* only;
  `AuthPrincipal`/`CurrentUser` reached the handler but was never threaded
  into `StorageApi`/`StorageService`).
- Effectively, **every bucket behaved as fully public to any authenticated
  caller** — the exact gap Workstream F exists to close.

This F1 implementation therefore had to build the "public bucket" flag itself
(a plain `is_public INT` column on `buckets`, default `false`/private) as
part of closing F1, rather than reusing a pre-existing flag. This is recorded
here per CLAUDE.md §0.6 item 6 (escalate a provably-wrong plan premise
honestly, in-doc, rather than silently building around it) — the roadmap
doc's "Already implemented" section should be read with this correction.

## What shipped (F1)

**Enforcement model: owner + public-bucket exemption + superuser/`service_role`
bypass**, evaluated in plain Rust at the `StorageService` layer — the "keep it
simple" fallback the task brief explicitly sanctions over a full policy-DDL
surface for storage.

- **Ownership.** `unidb-storage`'s `objects` table already had a
  `created_by TEXT` column (populated from the caller's JWT `sub` at
  `put_object` time for the outbox/audit trail) — this doubles as the F1
  owner field. No new object-level column was needed.
- **Public buckets.** New `buckets.is_public INT` column (default private).
  `POST /storage/buckets` accepts `"is_public": true`.
- **Reads** (`list_objects`, `get_object`, `presign_get` issuance): allowed if
  the bucket is public, the caller owns the object, or the caller bypasses.
  `list_objects` **filters** rather than erroring.
- **Writes/deletes** (`put_object`/`begin_upload` overwrite, `delete_object`):
  owner or bypass only; public-bucket status does not exempt writes.
- **Bypass:** a named `SUPERUSER`, the implicit embedded/no-`sub` caller,
  open/bootstrap mode, or a `service_role` JWT claim — resolved by
  `EngineHandle::storage_caller` via the existing `authz::RoleStore`
  (`is_superuser`/`effective_roles`), the same machinery every SQL statement
  already uses. A bypass is audited (`superuser_storage_bypass` /
  `service_role_storage_bypass`, `AuditLog::record_admin`).
- **Fail closed:** a private bucket with no matching rule denies (403
  `STORAGE_FORBIDDEN`).

**Plumbing:** the caller's identity now reaches storage as
`unidb::storage_api::StorageCaller` (`{subject, roles, is_superuser}`), built
once per request by `EngineHandle::storage_caller` from the request's
`AuthPrincipal` (already in Axum's request extensions via `require_jwt`) and
threaded through `StorageApi`/`StorageService`'s object-touching methods. No
parallel identity/role/audit system was introduced.

## What this intentionally does NOT do (follow-up)

- **No policy-DDL surface for storage** (e.g. `CREATE POLICY … ON storage
  FOR …`) — the task brief names this an acceptable, explicitly-noted
  follow-up when owner+public-bucket+bypass "closes the 'any authenticated
  user reads any private object' gap" solidly, which it does. A future
  iteration could route storage's `objects`/`buckets` table reads through
  `Engine::execute_sql_as_principal`'s RLS-aware plan path instead of the
  direct `metadata.rs` table helpers, letting `CREATE POLICY … ON objects`
  express richer per-bucket/per-tenant rules reusing `apply_rls` verbatim —
  scoped out here because `objects`/`buckets` are read via hand-rolled
  parameterized SQL in `metadata.rs`, not through the RLS-aware executor
  entry point, and rewiring that is a larger change than this task's remit.
- **No bucket-level authorization** (who may create/delete a *bucket*) —
  only object-level (F1's stated scope). Bucket CRUD is unchanged.
- **No schema migration for existing on-disk databases.** The new
  `buckets.is_public` column is added via `CREATE TABLE` DDL in
  `metadata::ensure_schema`, which is a no-op (`TableAlreadyExists`) against
  an already-initialized `buckets` table from before this change — such a
  table would lack the column. unidb has no `ALTER TABLE ADD COLUMN`
  surface; every `is_public` read defaults missing/absent to `false`
  (private) at the Rust level, so a pre-F1 deployment fails closed rather
  than erroring, but its buckets cannot be flagged public until reprovisioned.
  This mirrors the schema's existing evolution story (none) rather than
  inventing a migration mechanism out of scope for F1.

## Files touched

- `src/storage_api.rs` — `StorageCaller`, `StorageApiError::Forbidden`,
  `caller`/`is_public` added to the `StorageApi` trait.
- `src/server/engine_handle.rs` — `EngineHandle::storage_caller` (identity →
  roles/bypass resolution + audit).
- `src/server/storage.rs` — routes thread `AuthPrincipal` → `StorageCaller`
  into every object-touching call; `is_public` on bucket create; `owner` on
  object list.
- `src/server/error.rs` — `StorageApiError::Forbidden` → HTTP 403
  `STORAGE_FORBIDDEN`.
- `unidb-storage/src/metadata.rs` — `buckets.is_public` column,
  `BucketRow::is_public`, `get_bucket`.
- `unidb-storage/src/service.rs` — `can_read`/`can_write` gates, threaded
  through `list_objects`/`put_object`/`begin_upload`/`get_object`/
  `delete_object`/`presign_get`/`create_bucket`.
- `unidb-storage/src/api_impl.rs`, `unidb-storage/src/lib.rs` — trait impl +
  `StorageError::Forbidden` plumbing.
- `unidb-storage/tests/{round_trip,scale,crash_consistency,outbox_dispatcher}.rs`
  — updated call sites for the new `StorageCaller` parameter (behavior
  preserved via `StorageCaller::superuser()`/`::user(..)`).
- `tests/storage_authz_f1.rs` — new: owner read/write/delete, cross-user
  denial (403), public-bucket read exemption, `list_objects` filtering,
  presign-issuance denial, superuser/`service_role` bypass + audit-log proof.
- `docs/REST_API.md` — `/storage/*` authorization section + per-route 403s.
