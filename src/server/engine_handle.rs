//! The concurrent-writer bridge (P5.e-3). Since `Engine` is now `Send + Sync`
//! (P5.e-2), the server no longer funnels every write through one dedicated OS
//! thread. Instead `EngineHandle` holds an `Arc<Engine>` shared by **all**
//! request handlers, and each async method runs its (blocking) `Engine` call on
//! a tokio blocking-pool thread via [`tokio::task::spawn_blocking`]. Many
//! writers therefore execute in parallel across cores, coordinating only
//! through the engine's internal latches/locks (buffer-pool page latches, the
//! WAL append mutex, the row lock manager, MVCC snapshots).
//!
//! Durability under concurrency is handled by **group commit** inside the
//! engine: the handle opens the engine in deferred-sync mode (per-statement
//! mini-txn fsyncs are skipped), and `Engine::commit` forces the transaction's
//! commit record durable via `Wal::sync_up_to`, which coalesces concurrent
//! committers behind a single fsync (the leader-election barrier — see
//! `wal.rs`). So the more transactions commit at once, the fewer fsyncs they
//! collectively pay, and write throughput scales with concurrent writers rather
//! than hitting the old single-writer-thread ceiling.
//!
//! `EngineHandle::spawn` still opens the `Engine` **synchronously, on the
//! caller's thread**, so an `Engine::open` failure (corrupt control file, bad
//! WAL, etc.) surfaces immediately as `Result::Err` — exactly like every other
//! `Engine::open` call site — rather than being discovered by the first request.

use std::path::Path;
use std::sync::Arc;

use crate::{
    auth_principal::AuthPrincipal,
    catalog::{IndexKind, IndexStatus},
    error::{DbError, Result},
    format::Xid,
    graph::edges::Edge,
    heap::RowId,
    queue::Event,
    read_handle::ReadHandle,
    sql::executor::ExecResult,
    txn::IsolationLevel,
    Engine,
};

pub struct EngineHandle {
    /// The one shared, `Sync` engine every handler drives concurrently. `None`
    /// only after [`shutdown`](EngineHandle::shutdown) — post-shutdown calls
    /// then fail cleanly with [`DbError::EngineUnavailable`] instead of panicking.
    engine: Option<Arc<Engine>>,
    /// Concurrent read path (6b): reads bypass the engine's write coordination
    /// and run on this `Send + Sync` handle over shared state, so many readers
    /// execute in parallel with each other and with writers.
    read: ReadHandle,
}

impl EngineHandle {
    /// Open `Engine` on the calling thread (surfacing any open/recovery error
    /// immediately), enable group-commit deferral, and share it via `Arc`.
    pub fn spawn(dir: &Path, page_size: u32) -> Result<Self> {
        let engine = Engine::open(dir, page_size)?;
        // Group-committed force-log-at-commit is now the engine default (C1):
        // `Engine::open` defers per-statement fsyncs and `Engine::commit` forces
        // durability via the coalescing `Wal::sync_up_to` barrier. No explicit
        // `set_deferred_sync` call is needed here anymore.
        let read = engine.read_handle();
        let engine = Arc::new(engine);
        // Item 107: activate the async HNSW worker (item 67) for the served
        // engine — without this every INSERT into an HNSW-indexed table pays
        // the synchronous beam search (~6–18 ms) on the commit path (the
        // W4/W0 96× finding, 21 Jul bench). Freshness contract "a" (user
        // sign-off 2026-07-22): NEAR may lag committed rows by the queue
        // depth; the lag is exposed as `unidb_hnsw_queue_depth` and bounded
        // by the 4096-slot channel's backpressure.
        engine.spawn_hnsw_worker();
        // A3: start the background autovacuum launcher for the served instance
        // (default-on, policy-gated). The worker holds a `Weak<Engine>`, so this
        // Arc's eventual drop still tears the engine down cleanly.
        engine.spawn_autovacuum();
        // Item 34: start the stats-history ticker (5 s snapshot interval, 300-point
        // ring). Same Weak<Engine> / bounded-join pattern as autovacuum.
        engine.spawn_stats_ticker();
        Ok(Self {
            engine: Some(engine),
            read,
        })
    }

    /// Clone the shared engine `Arc` for a blocking task, or fail if the handle
    /// has been shut down.
    fn engine(&self) -> Result<Arc<Engine>> {
        self.engine
            .as_ref()
            .cloned()
            .ok_or(DbError::EngineUnavailable)
    }

    /// Expose the shared engine `Arc` for app-layer services (e.g.
    /// `unidb-storage` in item 31) that need the same `Arc<Engine>` instance.
    /// Returns `Err(EngineUnavailable)` after shutdown.
    pub fn engine_arc(&self) -> Result<Arc<Engine>> {
        self.engine()
    }

    /// Run one blocking `Engine` call on a tokio blocking-pool thread. This is
    /// the single choke point that turns every synchronous `Engine` method into
    /// a concurrency-safe async one; N of these run in parallel across the pool.
    async fn on_engine<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Engine) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let engine = self.engine()?;
        // Carry the request's correlation id onto the blocking pool thread so
        // engine-core logging (slow-query, audit) can tag it (item 22, L2).
        let request_id = crate::server::correlation::current_request_id();
        tokio::task::spawn_blocking(move || {
            let _corr = crate::observability::set_request_id(request_id);
            f(&engine)
        })
        .await
        .map_err(|_| DbError::EngineUnavailable)?
    }

    /// Read one row by [`RowId`] on the concurrent read path (6b): no xid, no
    /// WAL. Runs on a blocking pool thread since the read briefly locks shared
    /// state.
    pub async fn get_row(&self, row_id: RowId) -> Result<Vec<u8>> {
        let read = self.read.clone();
        let request_id = crate::server::correlation::current_request_id();
        tokio::task::spawn_blocking(move || {
            let _corr = crate::observability::set_request_id(request_id);
            read.get(row_id)
        })
        .await
        .map_err(|_| DbError::EngineUnavailable)?
    }

    /// Execute read-only SQL (`SELECT`) on the concurrent read path (6b) as the
    /// embedded/no-user superuser. Policies containing `current_user` are skipped.
    /// The caller must have classified the SQL as concurrent-readable (see
    /// [`crate::read_handle::is_concurrent_read_sql`]); a non-read statement
    /// returns [`DbError::SqlPlan`].
    pub async fn execute_sql_read(&self, sql: String) -> Result<Vec<ExecResult>> {
        let read = self.read.clone();
        let request_id = crate::server::correlation::current_request_id();
        tokio::task::spawn_blocking(move || {
            let _corr = crate::observability::set_request_id(request_id);
            read.execute_sql(&sql)
        })
        .await
        .map_err(|_| DbError::EngineUnavailable)?
    }

    /// Execute read-only SQL (`SELECT`) on the concurrent read path (6b) under
    /// a named user identity (item 103). Superusers and the no-`sub` path
    /// (`user = None`) bypass `current_user`-referencing policies. Regular users
    /// have policies applied with `current_user` substituted.
    ///
    /// Carries no verified claims — prefer
    /// [`Self::execute_sql_read_as_principal`] when `auth.jwt()`-referencing
    /// policies (item 122) must resolve correctly.
    pub async fn execute_sql_read_as(
        &self,
        user: Option<String>,
        sql: String,
    ) -> Result<Vec<ExecResult>> {
        let read = self.read.clone();
        let request_id = crate::server::correlation::current_request_id();
        tokio::task::spawn_blocking(move || {
            let _corr = crate::observability::set_request_id(request_id);
            read.execute_sql_as(user.as_deref(), &sql)
        })
        .await
        .map_err(|_| DbError::EngineUnavailable)?
    }

    /// Like [`Self::execute_sql_read_as`] but forwards a full [`AuthPrincipal`]
    /// (item 122, B1/B2) so `auth.uid()`/`auth.jwt() ->> '...'` RLS policies
    /// resolve correctly on the concurrent read path too — the same claims
    /// carried into `execute_sql_as_principal` on the writer path.
    pub async fn execute_sql_read_as_principal(
        &self,
        principal: AuthPrincipal,
        sql: String,
    ) -> Result<Vec<ExecResult>> {
        let read = self.read.clone();
        let request_id = crate::server::correlation::current_request_id();
        tokio::task::spawn_blocking(move || {
            let _corr = crate::observability::set_request_id(request_id);
            read.execute_sql_as_principal(&principal, &sql)
        })
        .await
        .map_err(|_| DbError::EngineUnavailable)?
    }

    pub async fn begin(&self, isolation: Option<IsolationLevel>) -> Result<Xid> {
        self.on_engine(move |e| match isolation {
            Some(iso) => e.begin_with_isolation(iso),
            None => e.begin(),
        })
        .await
    }

    pub async fn commit(&self, xid: Xid) -> Result<()> {
        self.on_engine(move |e| e.commit(xid)).await
    }

    pub async fn abort(&self, xid: Xid) -> Result<()> {
        self.on_engine(move |e| e.abort(xid)).await
    }

    pub async fn insert(&self, xid: Xid, data: Vec<u8>) -> Result<RowId> {
        self.on_engine(move |e| e.insert(xid, &data)).await
    }

    pub async fn get(&self, xid: Xid, row_id: RowId) -> Result<Vec<u8>> {
        self.on_engine(move |e| e.get(xid, row_id)).await
    }

    pub async fn update(&self, xid: Xid, row_id: RowId, new_data: Vec<u8>) -> Result<RowId> {
        self.on_engine(move |e| e.update(xid, row_id, &new_data))
            .await
    }

    pub async fn delete(&self, xid: Xid, row_id: RowId) -> Result<()> {
        self.on_engine(move |e| e.delete(xid, row_id)).await
    }

    pub async fn execute_sql(&self, xid: Xid, sql: String) -> Result<Vec<ExecResult>> {
        self.on_engine(move |e| e.execute_sql(xid, &sql)).await
    }

    pub async fn execute_sql_params(
        &self,
        xid: Xid,
        sql: String,
        params: Vec<crate::sql::logical::Literal>,
    ) -> Result<Vec<ExecResult>> {
        self.on_engine(move |e| e.execute_sql_params(xid, &sql, &params))
            .await
    }

    /// Like [`Self::execute_sql_params`] but threads a full [`AuthPrincipal`]
    /// through so RLS/`current_user()`/`auth.uid()`/`auth.jwt()` resolve under
    /// the caller's identity (item 123, Workstream C1) — see
    /// [`Engine::execute_sql_params_as_principal`] for why the bare params
    /// path can't be reused for this (it runs with no caller identity, so a
    /// `current_user`-referencing RLS policy fails closed regardless of who
    /// is actually calling). Used by the auto-REST layer
    /// (`server::rest_resource`) for every translated request.
    pub async fn execute_sql_params_as_principal(
        &self,
        principal: AuthPrincipal,
        xid: Xid,
        sql: String,
        params: Vec<crate::sql::logical::Literal>,
    ) -> Result<Vec<ExecResult>> {
        self.on_engine(move |e| e.execute_sql_params_as_principal(&principal, xid, &sql, &params))
            .await
    }

    pub async fn execute_cypher(&self, xid: Xid, query: String) -> Result<Vec<ExecResult>> {
        self.on_engine(move |e| e.execute_cypher(xid, &query)).await
    }

    /// Execute SQL as a named user (P6.e), enforcing privileges + handling auth
    /// DDL. `user == None` is the embedded superuser.
    pub async fn execute_sql_as(
        &self,
        user: Option<String>,
        xid: Xid,
        sql: String,
    ) -> Result<Vec<ExecResult>> {
        self.on_engine(move |e| e.execute_sql_as(user.as_deref(), xid, &sql))
            .await
    }

    /// Like [`Self::execute_sql_as`] but forwards a full [`AuthPrincipal`]
    /// (auth seam) instead of a bare subject string. `principal.claims`/
    /// `principal.roles` are carried down into the engine but are not yet
    /// consumed by any policy logic.
    pub async fn execute_sql_as_principal(
        &self,
        principal: AuthPrincipal,
        xid: Xid,
        sql: String,
    ) -> Result<Vec<ExecResult>> {
        self.on_engine(move |e| e.execute_sql_as_principal(&principal, xid, &sql))
            .await
    }

    /// Privilege pre-check for the read/param fast paths (P6.e).
    pub async fn authorize_sql(&self, user: Option<String>, sql: String) -> Result<()> {
        self.on_engine(move |e| e.authorize_sql(user.as_deref(), &sql))
            .await
    }

    /// Like [`Self::authorize_sql`] but principal-aware (item 122, B3) — a
    /// `service_role` claim skips the pre-check, mirroring
    /// `execute_sql_as_principal`'s bypass, so the fast-path pre-check can't
    /// reject a service_role token before the engine's own audited bypass
    /// ever runs.
    pub async fn authorize_sql_as_principal(
        &self,
        principal: AuthPrincipal,
        sql: String,
    ) -> Result<()> {
        self.on_engine(move |e| e.authorize_sql_as_principal(&principal, &sql))
            .await
    }

    /// Single-table grant check for REST routes that bypass SQL (item-24 Z3).
    pub async fn check_table_grant(
        &self,
        user: Option<String>,
        table: String,
        priv_: crate::authz::Privilege,
    ) -> Result<()> {
        self.on_engine(move |e| e.check_table_grant(user.as_deref(), &table, priv_))
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_edge(
        &self,
        xid: Xid,
        from_id: i64,
        to_id: i64,
        edge_type: String,
        props: String,
    ) -> Result<RowId> {
        self.on_engine(move |e| e.create_edge(xid, from_id, to_id, &edge_type, &props))
            .await
    }

    pub async fn delete_edge(&self, xid: Xid, row_id: RowId, from_id: i64) -> Result<()> {
        self.on_engine(move |e| e.delete_edge(xid, row_id, from_id))
            .await
    }

    pub async fn edges_from(&self, xid: Xid, from_id: i64) -> Result<Vec<Edge>> {
        self.on_engine(move |e| e.edges_from(xid, from_id)).await
    }

    pub async fn enable_events(&self, table: String) -> Result<()> {
        self.on_engine(move |e| e.enable_events(&table)).await
    }

    pub async fn is_events_enabled(&self, table: String) -> Result<bool> {
        self.on_engine(move |e| e.is_events_enabled(&table)).await
    }

    pub async fn disable_events(&self, table: String) -> Result<()> {
        self.on_engine(move |e| e.disable_events(&table)).await
    }

    pub async fn events_head_seq(&self) -> Result<i64> {
        self.on_engine(|e| e.events_head_seq()).await
    }

    pub async fn poll_events(
        &self,
        xid: Xid,
        consumer: String,
        limit: usize,
    ) -> Result<Vec<Event>> {
        self.on_engine(move |e| e.poll_events(xid, &consumer, limit))
            .await
    }

    /// E1 ephemeral live-tail cursor (item 20): events past `after_seq`, no
    /// durable consumer touched. Backs `Last-Event-ID`/`from_seq` resume.
    pub async fn poll_events_after(
        &self,
        xid: Xid,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<Event>> {
        self.on_engine(move |e| e.poll_events_after(xid, after_seq, limit))
            .await
    }

    /// item E1 (Workstream E, per-subscriber realtime authorization):
    /// filter + column-project one poll's events for delivery to `principal`
    /// over `/events/subscribe` — see [`Engine::filter_realtime_events`]'s
    /// doc comment for exactly what this reuses (RLS substitution + grants)
    /// and its fail-closed guarantees. Infallible on the `Engine` side (a
    /// per-event ambiguity just drops that event); the `Result` here is only
    /// for `EngineHandle`'s post-shutdown/blocking-pool-join failure mode,
    /// matching every other wrapper in this file.
    pub async fn filter_realtime_events(
        &self,
        principal: AuthPrincipal,
        events: Vec<Event>,
    ) -> Result<Vec<Event>> {
        self.on_engine(move |e| Ok(e.filter_realtime_events(&principal, events)))
            .await
    }

    /// Q2 (item 26): current commit generation — callers snapshot this before
    /// processing a batch so they can detect the NEXT commit even if it fires
    /// before the `wait_event_commit` call.
    pub fn event_commit_gen(&self) -> u64 {
        self.engine
            .as_ref()
            .map(|e| e.event_commit_gen())
            .unwrap_or(0)
    }

    /// Q2 (item 26): block (on a spawn_blocking thread) until a new commit
    /// occurs or `timeout` elapses, then return the new generation. Use this
    /// instead of a fixed sleep to reduce latency and CPU on idle streams.
    pub async fn wait_event_commit(&self, known_gen: u64, timeout: std::time::Duration) -> u64 {
        let Ok(engine) = self.engine() else {
            return known_gen;
        };
        tokio::task::spawn_blocking(move || engine.wait_event_commit_blocking(known_gen, timeout))
            .await
            .unwrap_or(known_gen)
    }

    pub async fn ack_events(&self, xid: Xid, consumer: String, up_to_seq: i64) -> Result<()> {
        self.on_engine(move |e| e.ack_events(xid, &consumer, up_to_seq))
            .await
    }

    pub async fn vacuum_events(&self, xid: Xid) -> Result<usize> {
        self.on_engine(move |e| e.vacuum_events(xid)).await
    }

    pub async fn set_column_index(
        &self,
        table: String,
        column: String,
        kind: Option<IndexKind>,
    ) -> Result<()> {
        self.on_engine(move |e| e.set_column_index(&table, &column, kind))
            .await
    }

    pub async fn index_status(&self, table: String, column: String) -> Option<IndexStatus> {
        let Ok(engine) = self.engine() else {
            return None;
        };
        tokio::task::spawn_blocking(move || engine.index_status(&table, &column))
            .await
            .unwrap_or(None)
    }

    pub async fn checkpoint(&self) -> Result<()> {
        self.on_engine(|e| e.checkpoint()).await
    }

    /// Superuser gate for admin routes (R3): `Ok` for the implicit superuser
    /// (no `sub`), a named `SUPERUSER`, or open/bootstrap mode.
    pub async fn ensure_superuser(&self, user: Option<String>) -> Result<()> {
        self.on_engine(move |e| e.ensure_superuser(user.as_deref()))
            .await
    }

    /// Resolve a caller's storage authorization context (item 120, Workstream
    /// F1 — per-object storage authz): effective roles plus the exact same
    /// bypass decision the SQL RLS path uses (`is_effective_superuser(user)
    /// || is_service_role`, see `Engine::is_effective_superuser` /
    /// `ReadHandle::execute_sql_inner`), reusing `authz::RoleStore` — no
    /// parallel identity/role system for `/storage/*`. A bypass is audited
    /// exactly like `service_role_rls_bypass` (item-103 lesson):
    /// `AuditLog::record_admin` no-ops for the implicit embedded caller
    /// (`subject == None`) and only logs named callers, so this call is
    /// cheap and safe to make on every storage request.
    pub async fn storage_caller(
        &self,
        principal: AuthPrincipal,
    ) -> Result<crate::storage_api::StorageCaller> {
        self.on_engine(move |e| {
            let effective_roles = e
                .authz
                .effective_roles(principal.subject.as_deref(), &principal.claims);
            let is_service_role = effective_roles
                .iter()
                .any(|r| r == crate::authz::SERVICE_ROLE);
            let is_superuser = match principal.subject.as_deref() {
                None => true,
                Some(u) => e.authz.is_superuser(u) || !e.authz.has_users(),
            };
            let bypass = is_superuser || is_service_role;
            if bypass {
                let action = if is_service_role {
                    "service_role_storage_bypass"
                } else {
                    "superuser_storage_bypass"
                };
                e.audit
                    .record_admin(principal.subject.as_deref(), None, action, "", true);
            }
            Ok(crate::storage_api::StorageCaller {
                subject: principal.subject,
                roles: effective_roles,
                is_superuser: bypass,
            })
        })
        .await
    }

    /// Whether any user exists in the role store — `false` = open/bootstrap mode
    /// (RLS policies are inactive).  Used by `GET /auth/meta`.
    pub async fn has_users(&self) -> bool {
        self.on_engine(|e| Ok(e.authz.has_users()))
            .await
            .unwrap_or(false)
    }

    /// Return a snapshot of all users (name, is_superuser) for `GET /auth/whoami`.
    pub async fn user_snapshot(&self) -> Vec<(String, bool)> {
        self.on_engine(|e| Ok(e.authz.users()))
            .await
            .unwrap_or_default()
    }

    /// Roles and table-level grants for a user (name, table, privileges).
    /// Used by `GET /auth/whoami`.
    pub async fn user_grants(&self, user: String) -> Vec<(String, Vec<String>)> {
        self.on_engine(move |e| Ok(e.authz.table_grants_for(&user)))
            .await
            .unwrap_or_default()
    }

    /// Roles a user is a member of (transitively).
    pub async fn user_roles(&self, user: String) -> Vec<String> {
        self.on_engine(move |e| Ok(e.authz.roles_for(&user)))
            .await
            .unwrap_or_default()
    }

    /// Verify a login password (item 121, A2). `false` covers unknown user,
    /// no stored credential, and wrong password alike (no user-enumeration
    /// oracle) — see [`crate::authz::RoleStore::verify_password`]. A
    /// bottom-of-the-stack engine error (should not happen for this
    /// read-only check) also maps to `false`, so a caller never has to
    /// distinguish "verify failed" from "verify errored" — both mean "don't
    /// log this request in."
    pub async fn verify_password(&self, user: String, password: String) -> bool {
        self.on_engine(move |e| Ok(e.verify_password(&user, &password)))
            .await
            .unwrap_or(false)
    }

    /// Create a new non-superuser user with a password credential (item 121,
    /// A3 — `POST /auth/signup`). Errors (e.g. duplicate username) propagate
    /// as-is; the caller maps them to an HTTP status via [`ApiError`](
    /// crate::server::error::ApiError).
    pub async fn create_user_with_password(&self, user: String, password: String) -> Result<()> {
        self.on_engine(move |e| e.create_user_with_password(&user, &password))
            .await
    }

    /// Issue a fresh refresh-token session for `user` (item 121, A4). Returns
    /// `(raw_refresh_token, expires_at_unix_secs)`.
    pub async fn create_session(&self, user: String) -> Result<(String, u64)> {
        self.on_engine(move |e| e.create_session(&user)).await
    }

    /// Verify a raw refresh token (item 121, A4). `None` uniformly covers
    /// unknown/expired/revoked — see [`crate::authz::RoleStore::
    /// verify_session`].
    pub async fn verify_session(&self, raw_token: String) -> Option<String> {
        self.on_engine(move |e| Ok(e.verify_session(&raw_token)))
            .await
            .unwrap_or(None)
    }

    /// Verify + rotate a refresh token (item 121, A4 — `POST /auth/refresh`).
    /// `Ok(None)` covers unknown/expired/revoked uniformly; `Err` only for a
    /// genuine persistence failure.
    pub async fn rotate_session(&self, raw_token: String) -> Result<Option<(String, String, u64)>> {
        self.on_engine(move |e| e.rotate_session(&raw_token)).await
    }

    /// Revoke a refresh-token session, idempotently (item 121, A4 —
    /// `POST /auth/logout`).
    pub async fn revoke_session(&self, raw_token: String) -> Result<()> {
        self.on_engine(move |e| e.revoke_session(&raw_token)).await
    }

    /// Username owning `session_id`, if any (item 4 — `DELETE
    /// /auth/sessions/{id}`'s self/superuser ownership gate).
    pub async fn session_owner(&self, session_id: String) -> Option<String> {
        self.on_engine(move |e| Ok(e.session_owner(&session_id)))
            .await
            .unwrap_or(None)
    }

    /// Revoke a session by its opaque id, idempotently (item 4 —
    /// `DELETE /auth/sessions/{id}`). Callers must already have checked the
    /// caller may act on this session (superuser, or `session_owner` ==
    /// caller) — this is the unrestricted mutation, mirroring
    /// `revoke_session`'s own no-op-on-unknown-id posture.
    pub async fn revoke_session_by_id(&self, session_id: String) -> Result<()> {
        self.on_engine(move |e| e.revoke_session_by_id(&session_id))
            .await
    }

    /// Install an RLS policy from a SQL predicate string (R3).
    pub async fn set_rls_policy_sql(&self, table: String, predicate: String) -> Result<()> {
        self.on_engine(move |e| e.set_rls_policy_sql(&table, &predicate))
            .await
    }

    /// `POST /admin/flush` (R3): force the WAL durable, then flush every
    /// dirty page. The WAL sync first keeps D5 satisfiable for pages whose
    /// records were deferred by group commit.
    pub async fn flush(&self) -> Result<()> {
        self.on_engine(|e| {
            e.sync_wal()?;
            e.flush()
        })
        .await
    }

    /// Snapshot every table's schema for `GET /tables` introspection (S1).
    pub async fn table_defs(&self) -> Result<Vec<crate::catalog::TableDef>> {
        self.on_engine(|e| Ok(e.table_defs())).await
    }

    /// A `pg_stat_*`-style activity + counter snapshot (P6.g).
    pub async fn stats(&self) -> Result<crate::EngineStats> {
        self.on_engine(|e| Ok(e.stats())).await
    }

    // ── Observability extras (item 34) ────────────────────────────────────────

    /// Update the slow-query threshold at runtime (item 34, Part A). Zero disables.
    pub async fn set_slow_query_threshold(&self, threshold_ms: u64) -> Result<()> {
        self.on_engine(move |e| {
            e.set_slow_query_threshold(std::time::Duration::from_millis(threshold_ms));
            Ok(())
        })
        .await
    }

    /// Update the WAL group-commit dwell window at runtime (item 101). Zero disables.
    pub async fn set_group_commit_window_us(&self, us: u64) -> Result<()> {
        self.on_engine(move |e| {
            e.set_group_commit_window_us(us);
            Ok(())
        })
        .await
    }

    /// Return up to `n` most-recent stats-history points (item 34, Part B).
    pub async fn stats_history(&self, n: usize) -> Result<Vec<crate::StatsHistoryPoint>> {
        self.on_engine(move |e| Ok(e.stats_history_snapshot(n)))
            .await
    }

    // ── Replication slots + WAL shipping (P6.b) ────────────────────────────────

    pub async fn create_replication_slot(
        &self,
        name: String,
        kind: crate::replication::SlotKind,
    ) -> Result<crate::replication::SlotInfo> {
        self.on_engine(move |e| e.create_replication_slot(&name, kind))
            .await
    }

    pub async fn drop_replication_slot(&self, name: String) -> Result<()> {
        self.on_engine(move |e| e.drop_replication_slot(&name))
            .await
    }

    pub async fn advance_replication_slot(&self, name: String, lsn: u64) -> Result<()> {
        self.on_engine(move |e| e.advance_replication_slot(&name, lsn))
            .await
    }

    pub async fn replication_slots(&self) -> Result<Vec<crate::replication::SlotInfo>> {
        self.on_engine(|e| Ok(e.replication_slots())).await
    }

    /// Ship the WAL record stream after `from_lsn` as framed bytes (P6.b), for a
    /// replica to decode + apply. Returns the primary's current tail LSN too, so
    /// the caller knows where the batch ends without decoding it.
    pub async fn ship_wal(&self, from_lsn: u64) -> Result<(u64, Vec<u8>)> {
        self.on_engine(move |e| Ok((e.wal_current_lsn(), e.ship_wal(from_lsn)?)))
            .await
    }

    /// Bulk-insert `rows` into `table` in one transaction (item 32).
    ///
    /// Builds a parameterized `INSERT INTO {table} ({cols}) VALUES ($1, …)`
    /// once, then loops `execute_prepared` for each row, and commits once.
    /// Returns the count of inserted rows. On any engine error the transaction
    /// is aborted — the entire batch is atomic (all-or-nothing).
    ///
    /// The column names and rows are validated by the caller (`bulk.rs`) before
    /// this is called, so identifiers here are already safe to interpolate.
    pub async fn bulk_insert(
        &self,
        table: String,
        columns: Vec<String>,
        rows: Vec<Vec<crate::sql::logical::Literal>>,
    ) -> Result<u64> {
        if rows.is_empty() {
            return Ok(0);
        }
        self.on_engine(move |engine| {
            let placeholders = (1..=columns.len())
                .map(|i| format!("${i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let col_list = columns.join(", ");
            let sql = format!("INSERT INTO {table} ({col_list}) VALUES ({placeholders})");
            let prepared = engine.prepare(&sql)?;
            let xid = engine.begin()?;
            let mut inserted = 0u64;
            let result: Result<()> = (|| {
                for params in &rows {
                    engine.execute_prepared(xid, &prepared, params)?;
                    inserted += 1;
                }
                Ok(())
            })();
            match result {
                Ok(()) => {
                    engine.commit(xid)?;
                    Ok(inserted)
                }
                Err(e) => {
                    let _ = engine.abort(xid);
                    Err(e)
                }
            }
        })
        .await
    }

    /// Release the shared engine. Every write already made itself durable at
    /// commit (group commit forces the WAL fsync before `commit` returns), so
    /// there is nothing to flush here; dropping the last `Arc<Engine>` closes
    /// its files. Idempotent — a second call is a harmless no-op.
    pub fn shutdown(&mut self) {
        // Dropping the `Arc` releases the engine once no in-flight blocking task
        // still holds a clone. Belt-and-suspenders: force any deferred WAL bytes
        // durable first, in case a non-commit write path deferred a flush.
        if let Some(engine) = self.engine.take() {
            // Item 118: drain the async HNSW worker and clear the crash-recovery
            // dirty marker BEFORE releasing the engine, so a graceful restart
            // skips reconciliation (O(1) open). Must run here, not in Engine's
            // Drop — the worker's Weak<Engine> can no longer upgrade once the
            // Arc is gone, so it could not drain the queue tail during Drop.
            engine.flush_hnsw_for_shutdown();
            let _ = engine.sync_wal();
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::tempdir;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn engine_is_send_sync() {
        assert_send_sync::<Engine>();
    }

    #[tokio::test]
    async fn round_trips_begin_insert_commit_get() {
        let dir = tempdir().unwrap();
        let handle = EngineHandle::spawn(dir.path(), 0).unwrap();

        let xid = handle.begin(None).await.unwrap();
        let row_id = handle.insert(xid, b"hello".to_vec()).await.unwrap();
        handle.commit(xid).await.unwrap();

        let xid2 = handle.begin(None).await.unwrap();
        let data = handle.get(xid2, row_id).await.unwrap();
        assert_eq!(data, b"hello");
    }

    #[tokio::test]
    async fn shutdown_releases_engine_and_is_idempotent() {
        let dir = tempdir().unwrap();
        let mut handle = EngineHandle::spawn(dir.path(), 0).unwrap();

        let start = std::time::Instant::now();
        handle.shutdown();
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "shutdown must return well within its bound"
        );
        handle.shutdown(); // second call must be a harmless no-op

        // Post-shutdown calls fail cleanly rather than panicking.
        assert!(matches!(
            handle.begin(None).await,
            Err(DbError::EngineUnavailable)
        ));

        // A fresh `Engine::open` against the same directory must succeed.
        Engine::open(dir.path(), 0).unwrap();
    }

    /// Many concurrent writers on the shared engine: no lost updates, no torn
    /// state, no deadlock hang. Each of N tasks inserts its own row in its own
    /// transaction; afterwards every row must be readable and distinct.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_writers_all_commit() {
        let dir = tempdir().unwrap();
        let handle = Arc::new(EngineHandle::spawn(dir.path(), 0).unwrap());

        let n = 200u32;
        let mut tasks = Vec::new();
        for i in 0..n {
            let h = handle.clone();
            tasks.push(tokio::spawn(async move {
                let xid = h.begin(None).await.unwrap();
                let rid = h
                    .insert(xid, format!("row-{i}").into_bytes())
                    .await
                    .unwrap();
                h.commit(xid).await.unwrap();
                rid
            }));
        }
        let mut rids = Vec::new();
        for t in tasks {
            rids.push(t.await.unwrap());
        }
        assert_eq!(rids.len() as u32, n);

        // Every committed row is durable and readable with its own contents.
        let xid = handle.begin(None).await.unwrap();
        for (i, rid) in rids.iter().enumerate() {
            let data = handle.get(xid, *rid).await.unwrap();
            assert_eq!(data, format!("row-{i}").into_bytes());
        }
        handle.commit(xid).await.unwrap();
    }
}
