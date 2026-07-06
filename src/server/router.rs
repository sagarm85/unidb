//! `build_router` assembles every route onto one `axum::Router`, layered
//! with `tower-http`'s trace/CORS/timeout middleware. Auth (M5.c) and
//! metrics (M5.c) layers attach here too once they exist.

use axum::{
    http::StatusCode,
    routing::{delete, get, post},
    Router,
};
use tower_http::{cors::CorsLayer, timeout::TimeoutLayer, trace::TraceLayer};

use crate::server::{handlers, AppState};

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/txn/begin", post(handlers::post_txn_begin))
        .route("/sql", post(handlers::post_sql))
        .route("/cypher", post(handlers::post_cypher))
        .route("/rows", post(handlers::post_row))
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
        .route("/events/ack", post(handlers::post_events_ack))
        .route("/checkpoint", post(handlers::post_checkpoint))
        .layer(TraceLayer::new_for_http())
        .layer(CorsLayer::permissive())
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(30),
        ))
        .with_state(state)
}
