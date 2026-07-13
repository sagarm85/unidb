//! `GET /events/subscribe` (M5.c; E1 framing, item 20): a server-side polling
//! loop that calls into the M4 event queue on an interval and forwards new
//! events as SSE frames.
//!
//! **State this plainly: this is "server polls, pushes to client," not
//! true WAL-level push.** `poll_events` has no wake primitive — there is
//! no way for the storage layer to notify anyone when a new event
//! commits — so "subscribe" here means the server pays the queue read's
//! own cost (linear in `__events__`'s total size, no predicate pushdown,
//! per M4's benchmark finding) once per polling interval, per connected
//! subscriber. That is a real, multiplicative cost — `N` subscribers ×
//! poll interval × read cost — not a free abstraction, and is quantified
//! directly in M5.d's benchmarks rather than left as a qualitative concern.
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

use std::{convert::Infallible, time::Duration};

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    response::sse::{Event as SseEvent, KeepAlive, Sse},
};
use futures_util::Stream;
use serde::Deserialize;

use crate::server::AppState;

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
}

fn default_limit() -> usize {
    100
}

fn default_interval_ms() -> u64 {
    500
}

pub async fn get_events_subscribe(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SubscribeParams>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    // Standard SSE reconnect: the browser replays its last-seen id here so the
    // server can resume strictly after it. Wins over `from_seq` when both set.
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok());

    let stream = async_stream::stream! {
        let mut interval = tokio::time::interval(Duration::from_millis(params.interval_ms.max(1)));
        // Ephemeral cursor: Last-Event-ID header > from_seq query > 0.
        let mut cursor = last_event_id.or(params.from_seq).unwrap_or(0);
        loop {
            interval.tick().await;

            // Each poll is its own short-lived read-only transaction — the
            // queue read requires an xid/snapshot like every other Engine
            // method, but there is no multi-tick session to keep open. A
            // transient EngineUnavailable/begin failure here just means "try
            // again next tick," not a fatal stream error — matching the
            // general engine-availability posture of this server (a dead
            // writer thread is an out-of-scope-for-v1 failure mode).
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

            // Optional per-connection table filter (E1). Applied after the
            // read since the queue has no predicate pushdown regardless.
            let events: Vec<_> = match &params.table {
                Some(t) => events.into_iter().filter(|e| &e.table_name == t).collect(),
                None => events,
            };
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
                let payload = serde_json::to_string(&event).unwrap_or_else(|_| "{}".into());
                yield Ok(SseEvent::default().id(seq.to_string()).event(op).data(payload));
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}
