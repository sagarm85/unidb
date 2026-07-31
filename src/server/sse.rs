//! `GET /events/subscribe` (M5.c; E1 framing, item 20): a server-side loop that
//! calls into the M4 event queue and forwards new events as SSE frames.
//!
//! **Q2 (item 26): push notification.** The loop now blocks on the engine's
//! commit condvar (`wait_event_commit`) instead of spinning on a fixed timer.
//! The stream wakes on each commit (via `EventWake`), polls `poll_events`/
//! `poll_events_after`, and forwards new events immediately. `interval_ms` is
//! retained as the maximum wait (fallback for idle periods and reconnect
//! catch-up). Combined with Q1's O(log n + returned) seq-index poll, an idle
//! subscriber blocks on the condvar at zero CPU cost; a busy one processes
//! each commit's events with single-digit-millisecond latency.
//!
//! Per-subscriber cost is now O(log n + new events per commit) instead of
//! O(total __events__ rows) × N subscribers × ticks per second — the
//! multiplicative cost that was the M4/item-20 known limitation.
//!
//! ## Two modes, one route (E1)
//!
//! - **Durable consumer** (`?consumer=<name>`, the M4/M5 default): reads via
//!   [`poll_events`], which resumes from the consumer's **durable** offset.
//!   Delivery is **at-least-once** — un-acked events are re-yielded every tick
//!   until the client acks over the ordinary `POST /events/ack` route (acks
//!   never travel over the SSE connection — the locked M5 decision), so a
//!   reconnect resumes exactly where the durable offset sits.
//! - **Ephemeral live-tail** (no `consumer`; browser `EventSource`): reads via
//!   [`poll_events_after`] with a per-connection in-memory cursor, so nothing
//!   is written to `__consumers__`. The cursor starts at the standard SSE
//!   `Last-Event-ID` reconnect header if present, else the explicit `from_seq`
//!   query param (studio offset-scrubbing / replay-from-offset), else `0`.
//!   Delivery is **at-most-once** across a dropped connection past the last
//!   frame the browser saw — which is exactly what `Last-Event-ID` is for:
//!   the browser reports its last id and we resume strictly after it.
//!
//! Both modes take an optional `table` filter so the studio can tail one
//! table's events (E1 AC) without seeing the whole stream. Heartbeats are
//! emitted by axum's SSE `KeepAlive` so proxies and browsers keep the idle
//! connection open.
//!
//! ## Per-subscriber authorization (Workstream E1, item 124)
//!
//! `/events/subscribe` is JWT-gated (the `require_jwt` middleware, same as
//! every other data-plane route) but that alone only proves *who* is asking
//! — it says nothing about which rows they're allowed to see. Before E1,
//! every authenticated caller received every event for the requested
//! table(s), including other tenants' rows on a shared table. E1 closes that
//! gap on the **delivery side only** — nothing about event *capture*
//! changes, and one subscriber's filtering never affects another's stream:
//!
//! 1. **Table-grant gate.** If a specific `?table=` filter is given, the
//!    subscription is rejected up front with `403 PERMISSION_DENIED` when the
//!    caller lacks `SELECT` on that table — a clear failure at subscribe
//!    time beats a silently-empty stream. When no `table` filter is given
//!    (the caller wants every table's events), there is no single
//!    subscribe-time table to gate on; the per-event check below (step 2)
//!    still guarantees no ungranted table's events reach that caller.
//! 2. **Every event**, every poll tick, is passed through
//!    [`crate::Engine::filter_realtime_events`] (via
//!    [`crate::server::engine_handle::EngineHandle::filter_realtime_events`])
//!    before being written to the SSE stream: table-grant re-check, the
//!    table's SELECT RLS policy evaluated against the event's row image
//!    (AFTER for INSERT/UPDATE, BEFORE for DELETE), superuser/`service_role`
//!    bypass (audited), and column-grant projection. See that method's doc
//!    comment for exactly which existing RLS/grant machinery it reuses (no
//!    parallel policy evaluator) and its fail-closed guarantees.
//!
//! **Cost, stated honestly:** this adds one extra blocking-pool round trip
//! (a catalog-locked pass with one `predicate_matches` call per event that
//! carries a policy) per non-empty poll tick per subscriber. It runs
//! entirely off the write/commit path — event capture, WAL, and every other
//! subscriber's stream are unaffected.

use std::{convert::Infallible, time::Duration};

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Extension,
};
use serde::Deserialize;

use crate::{authz::Privilege, server::error::ApiError, server::AppState, AuthPrincipal};

#[derive(Debug, Deserialize)]
pub struct SubscribeParams {
    /// Durable consumer name → at-least-once mode. Omit for the ephemeral
    /// live-tail (at-most-once) mode.
    #[serde(default)]
    pub consumer: Option<String>,
    /// Explicit start cursor for the ephemeral mode (offset scrubbing). The
    /// `Last-Event-ID` reconnect header takes precedence over this when both
    /// are present.
    #[serde(default)]
    pub from_seq: Option<i64>,
    /// Optional single-table filter (E1: "studio tails a table's events").
    #[serde(default)]
    pub table: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
    /// C2 (item 29): wire format. `"native"` (default) = full Event JSON;
    /// `"debezium"` = Debezium envelope; `"supabase"` = Supabase Realtime shape.
    #[serde(default = "default_format")]
    pub format: String,
}

fn default_format() -> String {
    "native".into()
}

fn default_limit() -> usize {
    100
}

fn default_interval_ms() -> u64 {
    500
}

pub async fn get_events_subscribe(
    State(state): State<AppState>,
    Extension(principal): Extension<AuthPrincipal>,
    headers: HeaderMap,
    Query(params): Query<SubscribeParams>,
) -> Result<Response, ApiError> {
    // E1 item 1 (table-grant gate): a caller subscribing to one specific
    // table gets a clean 403 up front when they lack SELECT on it, rather
    // than an SSE stream that silently never delivers anything. When no
    // `table` filter is given (all tables), there's no single table to gate
    // on here — the per-event grant re-check inside the loop below still
    // guarantees no ungranted table's events reach this subscriber.
    if let Some(table) = &params.table {
        state
            .engine
            .check_table_grant(principal.subject.clone(), table.clone(), Privilege::Select)
            .await?;
    }

    // Standard SSE reconnect: the browser replays its last-seen id here so the
    // server can resume strictly after it. Wins over `from_seq` when both set.
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok());

    let stream = async_stream::stream! {
        let fallback = Duration::from_millis(params.interval_ms.max(1));
        // Ephemeral cursor: Last-Event-ID header > from_seq query > 0.
        let mut cursor = last_event_id.or(params.from_seq).unwrap_or(0);
        // Q2: snapshot the generation before the first wait so we don't miss
        // a commit that happens between stream creation and the first wait.
        let mut known_gen = state.engine.event_commit_gen();
        loop {
            // Q2 push: block until a new commit or fallback timeout. An idle
            // stream blocks here at zero CPU; a busy one wakes per commit.
            known_gen = state.engine.wait_event_commit(known_gen, fallback).await;

            // Each poll is its own short-lived read-only transaction.
            let Ok(xid) = state.engine.begin(None).await else {
                continue;
            };
            let result = match &params.consumer {
                Some(consumer) => {
                    state
                        .engine
                        .poll_events(xid, consumer.clone(), params.limit)
                        .await
                }
                None => {
                    state
                        .engine
                        .poll_events_after(xid, cursor, params.limit)
                        .await
                }
            };
            let _ = state.engine.commit(xid).await;

            let Ok(events) = result else {
                continue;
            };

            metrics::counter!("unidb_sse_poll_cycles_total").increment(1);

            // Optional per-connection table filter (E1 AC). Applied after the
            // read since the queue has no predicate pushdown regardless.
            let events: Vec<_> = match &params.table {
                Some(t) => events.into_iter().filter(|e| &e.table_name == t).collect(),
                None => events,
            };
            if events.is_empty() {
                continue;
            }

            // E1 item 2/3/4: per-subscriber RLS + grant filter, fail closed.
            // Reuses the exact policy substitution/evaluation and grant
            // checks the query path uses — see
            // `crate::Engine::filter_realtime_events`'s doc comment. A
            // filter-call failure (engine unavailable) fails closed too:
            // deliver nothing this tick rather than the unfiltered batch.
            let events = state
                .engine
                .filter_realtime_events(principal.clone(), events)
                .await
                .unwrap_or_default();
            if events.is_empty() {
                continue;
            }
            metrics::counter!("unidb_sse_events_delivered_total").increment(events.len() as u64);

            for event in events {
                let seq = event.seq;
                // Advance the ephemeral cursor so the next tick reads strictly
                // past what we just emitted (durable mode leaves it untouched —
                // its progress lives in the acked consumer offset).
                if params.consumer.is_none() {
                    cursor = cursor.max(seq);
                }
                let op = event.op.clone();
                // C2 (item 29): serialize in the requested format.
                let payload =
                    crate::server::event_format::format_event(&event, &params.format);
                yield Ok::<_, Infallible>(SseEvent::default().id(seq.to_string()).event(op).data(payload));
            }
        }
    };

    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}
