//! `build_router` assembles every route onto one `axum::Router`. Data-plane
//! routes live in a `protected` sub-router wrapped with the verify-only JWT
//! middleware (`auth::require_jwt`); `GET /metrics` lives in a separate
//! `public` sub-router that never sees that layer — Prometheus scrapers
//! don't carry app-level bearer tokens (see `auth.rs`'s module doc). Both
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

use crate::server::{auth::JwtConfig, bulk, handlers, sse, storage, AppState};

pub fn build_router(
    state: AppState,
    jwt_config: JwtConfig,
    prometheus_layer: PrometheusMetricLayer<'static>,
    metric_handle: PrometheusHandle,
) -> Router {
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
    let metrics_state = state;
    let public = Router::new().route(
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
    );

    Router::new()
        .merge(protected)
        .merge(public)
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
