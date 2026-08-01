//! `build_router` assembles every route onto one `axum::Router`. Data-plane
//! routes live in a `protected` sub-router wrapped with the JWT middleware
//! (`auth::require_jwt`, HS256 or item 121 A6's RS256/ES256 depending on
//! config); `GET /metrics` and `GET /.well-known/jwks.json` (item 121 A6)
//! live in a separate `public` sub-router that never sees that layer —
//! neither a Prometheus scraper nor an external JWKS-fetching verifier
//! carries an app-level bearer token (see `auth.rs`'s module doc). Both
//! merge under one top-level `PrometheusMetricLayer` (so `/metrics`
//! requests themselves are counted too) plus `tower-http`'s trace/CORS/
//! timeout middleware.
//!
//! **The `PrometheusMetricLayer`/`PrometheusHandle` pair is a caller-owned
//! argument, not built inside this function.** `PrometheusMetricLayer::
//! pair()` installs a process-global `metrics` recorder — calling it more
//! than once in the same process panics ("Failed to set global recorder").
//! In production (`src/bin/unidb-server.rs`) `build_router` is only ever
//! called once, so this would never matter — but integration tests
//! (M5.d's `tests/server_*.rs`) spin up multiple independent test servers
//! *within one test binary process*. Accepting the pair as an argument
//! lets the test harness obtain it exactly once (e.g. via a `OnceLock`)
//! and reuse it across every test-local server, while production code
//! still gets the natural "call `pair()` once at startup" shape.

use axum::{
    http::StatusCode,
    routing::{delete, get, post, put},
    Router,
};
use axum_prometheus::{metrics_exporter_prometheus::PrometheusHandle, PrometheusMetricLayer};
use tower_http::{cors::CorsLayer, timeout::TimeoutLayer, trace::TraceLayer};

use crate::server::{
    auth::JwtConfig, bulk, graphql, handlers, rate_limit::AuthRateLimiter, realtime, rest_resource,
    sse, storage, AppState,
};

/// Production entry point: reads `UNIDB_AUTH_RATE_LIMIT` /
/// `UNIDB_AUTH_RATE_WINDOW_SECS` for the auth-mutation rate limiter (item
/// 121 I1), mirroring how [`router_timeout`] reads its own env var
/// internally rather than taking it as a parameter. Tests that need a fast,
/// deterministic limiter (instead of racing real env-var defaults) use
/// [`build_router_with_rate_limiter`] directly.
pub fn build_router(
    state: AppState,
    jwt_config: JwtConfig,
    prometheus_layer: PrometheusMetricLayer<'static>,
    metric_handle: PrometheusHandle,
) -> Router {
    build_router_with_rate_limiter(
        state,
        jwt_config,
        prometheus_layer,
        metric_handle,
        AuthRateLimiter::from_env(),
    )
}

/// [`build_router`] with an explicit [`AuthRateLimiter`] — the real
/// implementation. Kept separate so integration tests can inject a
/// short-window limiter without mutating process-global environment
/// variables (which would race other tests in the same test binary).
pub fn build_router_with_rate_limiter(
    state: AppState,
    jwt_config: JwtConfig,
    prometheus_layer: PrometheusMetricLayer<'static>,
    metric_handle: PrometheusHandle,
    auth_rate_limiter: AuthRateLimiter,
) -> Router {
    // Item 121 A6: computed once, before `jwt_config` is moved into the
    // `require_jwt` middleware below — the JWKS document never changes at
    // runtime (it mirrors whatever key material startup configured), so
    // there is nothing to recompute per-request.
    let jwks_document = jwt_config.jwks_document();

    let protected = Router::new()
        .route("/txn/begin", post(handlers::post_txn_begin))
        .route("/txn/{txn_id}/commit", post(handlers::post_txn_commit))
        .route("/txn/{txn_id}/rollback", post(handlers::post_txn_rollback))
        .route("/sql", post(handlers::post_sql))
        .route("/batch-sql", post(handlers::post_batch_sql))
        .route(
            "/sql/cursor/{cursor_id}",
            get(handlers::get_sql_cursor).delete(handlers::delete_sql_cursor),
        )
        .route("/cypher", post(handlers::post_cypher))
        .route("/rows", post(handlers::post_row))
        .route("/rows/batch", post(handlers::post_rows_batch))
        .route(
            "/rows/{page_id}/{slot}",
            get(handlers::get_row)
                .put(handlers::put_row)
                .delete(handlers::delete_row),
        )
        .route("/edges", post(handlers::post_edge))
        .route("/edges/{page_id}/{slot}", delete(handlers::delete_edge))
        .route("/edges/from/{from_id}", get(handlers::get_edges_from))
        .route("/indexes", post(handlers::post_index))
        .route(
            "/indexes/{table}/{column}/status",
            get(handlers::get_index_status),
        )
        .route("/tables", get(handlers::get_tables))
        // Item 32: NDJSON bulk-insert — one txn, one prepared stmt, N rows.
        // Generic data-loading primitive consistent with the Milestone-18 boundary:
        // operates on any user table, like Postgres COPY or /rows/batch.
        .route("/tables/{table}/bulk", post(bulk::post_tables_bulk))
        .route(
            "/tables/{table}/events",
            post(handlers::post_enable_events)
                .get(handlers::get_table_events_status)
                .delete(handlers::delete_table_events),
        )
        .route(
            "/tables/{table}/rls",
            axum::routing::put(handlers::put_table_rls),
        )
        // item-24 Z6: POST /auth/preview — run SQL as a named role, with RLS
        // applied, so an admin can preview what a specific user sees.
        .route("/auth/preview", post(handlers::post_auth_preview))
        // item 100: GET /auth/whoami — caller's identity + grants (JWT required).
        .route("/auth/whoami", get(handlers::get_auth_whoami))
        // item 4: DELETE /auth/sessions/{id} — revoke a specific session by
        // its opaque id (superuser/self gated; see the handler's doc comment).
        .route("/auth/sessions/{id}", delete(handlers::delete_auth_session))
        // item 127 (Workstream D4): TOTP MFA enroll/verify/disable — all
        // three act on the caller's own account and so require the same JWT
        // this whole `protected` sub-router already enforces. The MFA login
        // gate itself (`POST /auth/mfa/challenge`) is deliberately NOT here —
        // it runs *before* a session exists, alongside `/auth/login`, in the
        // rate-limited public router below.
        .route("/auth/mfa/enroll", post(handlers::post_mfa_enroll))
        .route("/auth/mfa/verify", post(handlers::post_mfa_verify))
        .route("/auth/mfa/disable", post(handlers::post_mfa_disable))
        .route("/events/head", get(handlers::get_events_head))
        .route("/events/subscribe", get(sse::get_events_subscribe))
        .route("/events/ack", post(handlers::post_events_ack))
        .route("/events/vacuum", post(handlers::post_events_vacuum))
        .route("/checkpoint", post(handlers::post_checkpoint))
        .route("/admin/flush", post(handlers::post_admin_flush))
        .route("/stats", get(handlers::get_stats))
        .route("/stats/history", get(handlers::get_stats_history))
        .route(
            "/config/slow_query_threshold_ms",
            put(handlers::put_config_slow_query_threshold_ms),
        )
        .route(
            "/config/group_commit_window_us",
            put(handlers::put_config_group_commit_window_us),
        )
        .route("/logs", get(handlers::get_logs))
        .route(
            "/replication/slots",
            post(handlers::post_replication_slot).get(handlers::get_replication_slots),
        )
        .route(
            "/replication/slots/{name}",
            delete(handlers::delete_replication_slot),
        )
        .route(
            "/replication/slots/{name}/advance",
            post(handlers::post_replication_slot_advance),
        )
        .route("/replication/stream", get(handlers::get_replication_stream))
        // ── Item 123 (Workstream C1): schema-derived auto REST API ────────
        // Same `require_jwt` layer as every other data-plane route below —
        // RLS/table/column-grant enforcement is inherited from the engine's
        // existing `POST /sql` enforcement path (see `rest_resource.rs`'s
        // module doc), not re-implemented here.
        .route(
            "/rest/v1/{table}",
            get(rest_resource::get_collection)
                .post(rest_resource::post_collection)
                .patch(rest_resource::patch_collection)
                .delete(rest_resource::delete_collection),
        )
        // C3: catalog-derived OpenAPI 3 document (feeds unidb-studio's
        // API-docs panel, G4). Mounted at both `/rest/v1` and `/rest/v1/` so
        // neither form 404s depending on whether the client includes the
        // trailing slash.
        .route("/rest/v1", get(rest_resource::get_openapi))
        .route("/rest/v1/", get(rest_resource::get_openapi))
        // ── Item 123 (Workstream C4): schema-derived GraphQL endpoint ─────
        // Same `require_jwt` layer as every other data-plane route — every
        // resolved field runs through the exact same enforced query path
        // `rest_resource`/`POST /sql` use (see `graphql.rs`'s module doc).
        .route("/graphql", post(graphql::post_graphql))
        // ── Item 132: Realtime Broadcast + Presence ────────────────────────
        // Same `require_jwt` layer as every other data-plane route below.
        // Item 140 adds an opt-in, role-based channel-authorization policy
        // gate ahead of each of these four routes (see `realtime.rs`'s
        // module doc and `docs/REST_API.md`) — a topic with no matching
        // policy still stays open to any authenticated principal unless
        // `UNIDB_REALTIME_REQUIRE_AUTHZ=1`.
        .route(
            "/realtime/broadcast/publish",
            post(realtime::post_broadcast_publish),
        )
        .route(
            "/realtime/broadcast/subscribe",
            get(realtime::get_broadcast_subscribe),
        )
        .route(
            "/realtime/presence/subscribe",
            get(realtime::get_presence_subscribe),
        )
        .route(
            "/realtime/presence/track",
            post(realtime::post_presence_track),
        )
        // item 140 admin surface: superuser-only channel-policy management.
        .route(
            "/realtime/policies",
            get(handlers::get_realtime_policies)
                .put(handlers::put_realtime_policy)
                .delete(handlers::delete_realtime_policy),
        )
        // ── Item 141: database webhooks (outbound HTTP on row change) ─────
        // Superuser-only admin surface, same posture as `/realtime/policies`
        // above; delivery itself happens off this router entirely (the
        // background worker in `server::webhooks`).
        .route(
            "/webhooks",
            get(handlers::get_webhooks).post(handlers::post_webhook),
        )
        .route("/webhooks/{id}", delete(handlers::delete_webhook))
        // ── Item 144: scheduled jobs (cron) ────────────────────────────────
        // Superuser-only admin surface, same posture as `/webhooks` above;
        // execution itself happens off this router entirely (the
        // background worker in `server::cron`).
        .route(
            "/cron/jobs",
            get(handlers::get_cron_jobs).post(handlers::post_cron_job),
        )
        .route("/cron/jobs/{name}", delete(handlers::delete_cron_job))
        // ── Item 142: Auth admin API (user management) ─────────────────────
        // Superuser-only admin surface, same posture as `/realtime/policies`/
        // `/webhooks` above — consolidated user management (list/get/create/
        // update/delete + ban + metadata), Supabase's `auth.admin`.
        .route(
            "/auth/admin/users",
            get(handlers::get_admin_users).post(handlers::post_admin_user),
        )
        .route(
            "/auth/admin/users/{id}",
            get(handlers::get_admin_user)
                .patch(handlers::patch_admin_user)
                .delete(handlers::delete_admin_user),
        )
        // ── Item 145: dev-inbox read route ──────────────────────────────────
        // Dev/testing aid, never for production — double-gated: 404 unless
        // the active email transport is the dev-inbox log transport, then
        // superuser-only (403 otherwise). See `handlers.rs`'s item-145
        // section doc for the full posture.
        .route(
            "/auth/dev-inbox",
            get(handlers::get_dev_inbox).delete(handlers::delete_dev_inbox),
        )
        // ── Item 31: storage service routes (/storage/*) ──────────────────
        // All 7 routes return 503 when AppState::storage is None (unconfigured).
        // C1 list / C2 create buckets
        .route(
            "/storage/buckets",
            get(storage::list_buckets).post(storage::create_bucket),
        )
        // C3 delete bucket (409 if non-empty)
        .route("/storage/buckets/{name}", delete(storage::delete_bucket))
        // C4 list objects with prefix + delimiter virtual-folder support
        .route("/storage/{bucket}/objects", get(storage::list_objects))
        // C5 put object (inline ≤ threshold; larger → presigned PUT ticket)
        // C6 delete object
        .route(
            "/storage/{bucket}/objects/{*key}",
            put(storage::put_object).delete(storage::delete_object),
        )
        // C7 presigned GET URL for direct browser download
        .route(
            "/storage/{bucket}/presign/{*key}",
            get(storage::presign_get),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            jwt_config,
            crate::server::auth::require_jwt,
        ))
        .with_state(state.clone());

    // `/metrics` (P6.g + item 21): the axum-prometheus HTTP metrics plus the
    // app-level engine gauges, refreshed from `Engine::stats()` on each scrape.
    // The engine captures everything lock-free into atomics/histograms on the
    // hot paths; this scrape handler is the only place that reads them back and
    // republishes them through the Prometheus facade (`metrics` crate), so a
    // scrape never perturbs the write path. Every metric name emitted here is
    // documented with its driven widget in `docs/engine_access_guide.md`.
    // item 100: public auth routes — no JWT middleware.
    // GET /auth/meta   → blank-slate discovery (open_mode, privilege types, catalog tables).
    // POST /auth/login → real password login (item 121 A1/A2: argon2id credential
    //   verification); issuance is now a first-class production capability
    //   (item 121 A5, UNIDB_JWT_SIGNING_KEY) as well as UNIDB_DEV_LOGIN=1.
    // item 128 (Workstream D1): OAuth 2.0 Authorization Code + PKCE social
    // login. Both routes are public (no JWT — they establish identity, they
    // don't presuppose it) and return 404 for a provider with no
    // UNIDB_OAUTH_<PROVIDER>_CLIENT_ID/_CLIENT_SECRET/_REDIRECT_URI
    // configured (`AppState::oauth`), same posture as dev-login/signup being
    // off by default. Not folded into `auth_rate_limited` below: unlike
    // login/signup/refresh, neither route accepts a guessable credential —
    // `authorize` takes no input at all, and `callback`'s `state`/`code` are
    // both high-entropy, single-use, server-validated tokens with no
    // meaningful guess surface for a fixed-window IP limiter to protect.
    let auth_public = Router::new()
        .route("/auth/meta", get(handlers::get_auth_meta))
        .route("/auth/logout", post(handlers::post_auth_logout))
        .route(
            "/auth/oauth/{provider}/authorize",
            get(handlers::get_oauth_authorize),
        )
        .route(
            "/auth/oauth/{provider}/callback",
            get(handlers::get_oauth_callback),
        )
        .with_state(state.clone());

    // item 121 I1: brute-force protection over exactly the three password-auth
    // mutation routes — never /sql, /metrics, /.well-known/jwks.json,
    // /auth/meta, or any read route (see `rate_limit.rs`'s module doc). The
    // limiter is its own `from_fn_with_state` layer (same shape as `require_jwt`
    // above), keyed by client IP via `ConnectInfo<SocketAddr>` — both
    // `unidb-server.rs` and the test harness serve this router through
    // `into_make_service_with_connect_info::<SocketAddr>()` so that extractor
    // resolves.
    let auth_rate_limited = Router::new()
        .route("/auth/login", post(handlers::post_auth_login))
        // item 121 A3: POST /auth/signup — 404s unless UNIDB_ALLOW_SIGNUP=1.
        .route("/auth/signup", post(handlers::post_auth_signup))
        // item 121 A4: refresh tokens + sessions + logout.
        .route("/auth/refresh", post(handlers::post_auth_refresh))
        // item 127 (Workstream D4): redeem an MFA login challenge for a real
        // session. Reachable with no bearer token (same as login/signup/
        // refresh) and guesses a 6-digit code, so it shares the exact same
        // brute-force protection — its own independent rate-limit bucket,
        // keyed by IP+path (see `rate_limit.rs`'s module doc; the body has
        // no `username` field to additionally key on, same as `/auth/refresh`).
        .route("/auth/mfa/challenge", post(handlers::post_mfa_challenge))
        // item 138: password-reset (recover/verify) + magic-link
        // (magiclink/magiclink/verify) email flows. `recover`/`magiclink`
        // guess nothing (they always 200, no-account-enumeration), but the
        // *verify* routes redeem a guessable-shaped opaque token, so all
        // four share the same brute-force protection as login/signup/
        // refresh/mfa-challenge above — same rationale as those routes.
        .route("/auth/recover", post(handlers::post_auth_recover))
        .route("/auth/verify", post(handlers::post_auth_verify))
        .route("/auth/magiclink", post(handlers::post_auth_magiclink))
        .route(
            "/auth/magiclink/verify",
            post(handlers::post_auth_magiclink_verify),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            auth_rate_limiter,
            crate::server::rate_limit::rate_limit_auth,
        ))
        .with_state(state.clone());

    let metrics_state = state;
    let public = Router::new()
        .route(
            "/metrics",
            get(move || {
                let handle = metric_handle.clone();
                let state = metrics_state.clone();
                async move {
                    if let Ok(stats) = state.engine.stats().await {
                        publish_engine_metrics(&stats);
                        // Server-session panel (item 12/21) — reads AppState, not
                        // the engine, so it lives here rather than in `stats()`.
                        metrics::gauge!("unidb_open_txn_sessions").set(state.sessions.len() as f64);
                        metrics::gauge!("unidb_open_cursors").set(state.cursors.len() as f64);
                        metrics::gauge!("unidb_idle_reaper_aborts_total")
                            .set(state.sessions.reaper_aborts() as f64);
                    }
                    handle.render()
                }
            }),
        )
        // item 121 A6: GET /.well-known/jwks.json — public, no JWT required
        // (a verifier fetching keys can't present one yet). Returns the
        // configured asymmetric public key as a JWK Set, or `{"keys":[]}`
        // when this server verifies HS256 only — see `JwtConfig::
        // jwks_document`'s doc comment for why the HS256 secret can never
        // leak through this route.
        .route(
            "/.well-known/jwks.json",
            get(move || {
                let doc = jwks_document.clone();
                async move { axum::Json(doc) }
            }),
        );

    Router::new()
        .merge(protected)
        .merge(public)
        .merge(auth_public)
        .merge(auth_rate_limited)
        .layer(prometheus_layer)
        .layer(TraceLayer::new_for_http())
        // Outermost app layer (item 22, L2): assign a `request_id` before auth
        // so even a rejected request is traceable, scope it as a task-local for
        // the engine bridge, and echo it back as `x-request-id`. Sits inside the
        // CORS/timeout tower layers but outside everything else.
        .layer(axum::middleware::from_fn(
            crate::server::correlation::assign_request_id,
        ))
        .layer(CorsLayer::permissive())
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            router_timeout(),
        ))
}

/// Global request timeout, overridable via `UNIDB_REQUEST_TIMEOUT_SECS`.
/// Default: 120 s — large enough for 100k-row bulk payloads on the `/tables/{name}/bulk`
/// endpoint (item 32). Set to 0 to disable entirely (development / local bulk tooling).
pub(crate) fn router_timeout() -> std::time::Duration {
    let secs: u64 = std::env::var("UNIDB_REQUEST_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    std::time::Duration::from_secs(secs)
}

/// Republish a `stats()` snapshot through the Prometheus facade (item 21).
/// Called only on a `/metrics` scrape — the engine already captured everything
/// lock-free into atomics/histograms on its hot paths, so this is a pure
/// read-and-set. Metric names (and the widget each drives) are catalogued in
/// `docs/engine_access_guide.md`'s widget-traceability table; keep the two in
/// sync when adding a metric here.
fn publish_engine_metrics(stats: &crate::EngineStats) {
    use metrics::gauge;

    // Commit-rate + durability-cost panel.
    gauge!("unidb_commits_total").set(stats.commits as f64);
    gauge!("unidb_aborts_total").set(stats.aborts as f64);
    gauge!("unidb_checkpoints_total").set(stats.checkpoints as f64);
    gauge!("unidb_wal_bytes").set(stats.wal_bytes as f64);
    gauge!("unidb_wal_fsyncs_total").set(stats.wal_fsyncs as f64);
    // Item 107: NEAR freshness lag (rows committed but not yet HNSW-indexed).
    gauge!("unidb_hnsw_queue_depth")
        .set(crate::hnsw_index::HNSW_QUEUE_DEPTH.load(std::sync::atomic::Ordering::Relaxed) as f64);
    gauge!("unidb_wal_fsync_p50_us").set(stats.wal_fsync_latency.p50_us as f64);
    gauge!("unidb_wal_fsync_p99_us").set(stats.wal_fsync_latency.p99_us as f64);

    // Query-latency panel: one p50/p99 pair per statement kind.
    let sl = &stats.statement_latency;
    for (kind, h) in [
        ("insert", &sl.insert),
        ("update", &sl.update),
        ("delete", &sl.delete),
        ("select", &sl.select),
    ] {
        gauge!("unidb_statement_latency_p50_us", "kind" => kind).set(h.p50_us as f64);
        gauge!("unidb_statement_latency_p99_us", "kind" => kind).set(h.p99_us as f64);
        gauge!("unidb_statement_count", "kind" => kind).set(h.count as f64);
    }

    // Cache-efficiency panel.
    let bp = &stats.bufferpool;
    gauge!("unidb_bufferpool_hits_total").set(bp.hits as f64);
    gauge!("unidb_bufferpool_misses_total").set(bp.misses as f64);
    gauge!("unidb_bufferpool_evictions_total").set(bp.evictions as f64);
    gauge!("unidb_bufferpool_hit_ratio").set(bp.hit_ratio);

    // Contention panel.
    gauge!("unidb_lock_waits_total").set(stats.locks.waits as f64);
    gauge!("unidb_deadlocks_total").set(stats.locks.deadlocks as f64);
    gauge!("unidb_lock_wait_p50_us").set(stats.locks.wait.p50_us as f64);
    gauge!("unidb_lock_wait_p99_us").set(stats.locks.wait.p99_us as f64);

    // Bloat-risk gauge (the item-16 postmortem metric — alert on this).
    gauge!("unidb_horizon_age_seconds").set(stats.horizon_age_secs);

    // Autovacuum / table-health.
    gauge!("unidb_autovacuum_runs_total").set(stats.autovacuums as f64);
    gauge!("unidb_dead_tuple_estimate").set(stats.dead_tuple_estimate as f64);
    gauge!("unidb_live_tuple_estimate").set(stats.live_tuple_estimate as f64);
    gauge!("unidb_autovacuum_last_run_epoch_secs").set(stats.last_autovacuum_epoch_secs as f64);
    for t in &stats.tables {
        gauge!("unidb_table_pages", "table" => t.name.clone()).set(t.pages as f64);
    }

    // Worker-governance panel (item 15).
    let w = &stats.parallel_workers;
    gauge!("unidb_parallel_worker_budget").set(w.global_max as f64);
    gauge!("unidb_parallel_workers_available").set(w.available as f64);
    gauge!("unidb_parallel_scans_total").set(w.parallel_scans as f64);
    gauge!("unidb_parallel_workers_granted_total").set(w.workers_granted as f64);
    gauge!("unidb_parallel_serial_fallbacks_total").set(w.serial_fallbacks as f64);

    // CDC subscription lag per consumer (item 29, C3).
    // Alert on unidb_subscription_lag_events{consumer="…"} > threshold.
    for lag in &stats.subscription_lag {
        let c = lag.consumer.clone();
        gauge!("unidb_subscription_lag_events", "consumer" => c.clone()).set(lag.lag_events as f64);
        gauge!("unidb_subscription_lag_seconds", "consumer" => c).set(lag.lag_seconds);
    }
}
