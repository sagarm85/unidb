//! `GET /events/subscribe` (M5.c): a server-side polling loop that calls
//! `poll_events` on an interval and forwards new events as SSE frames.
//!
//! **State this plainly: this is "server polls, pushes to client," not
//! true WAL-level push.** `poll_events` has no wake primitive — there is
//! no way for the storage layer to notify anyone when a new event
//! commits — so "subscribe" here means the server pays `poll_events`'s
//! own cost (linear in `__events__`'s total size, no predicate pushdown,
//! per M4's benchmark finding) once per polling interval, per connected
//! subscriber. That is a real, multiplicative cost — `N` subscribers ×
//! poll interval × `poll_events` cost — not a free abstraction, and is
//! quantified directly in M5.d's benchmarks rather than left as a
//! qualitative concern. Acks happen over the ordinary `POST /events/ack`
//! (`handlers.rs`), never over the SSE connection itself — the locked
//! design decision from the M5 plan.

use std::{convert::Infallible, time::Duration};

use axum::{
    extract::{Query, State},
    response::sse::{Event as SseEvent, KeepAlive, Sse},
};
use futures_util::Stream;
use serde::Deserialize;

use crate::server::AppState;

#[derive(Debug, Deserialize)]
pub struct SubscribeParams {
    pub consumer: String,
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
    Query(params): Query<SubscribeParams>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let stream = async_stream::stream! {
        let mut interval = tokio::time::interval(Duration::from_millis(params.interval_ms.max(1)));
        loop {
            interval.tick().await;

            // Each poll is its own short-lived read-only transaction —
            // `poll_events` requires an xid/snapshot like every other
            // Engine method, but there is no multi-tick session to keep
            // open. A transient EngineUnavailable/begin failure here just
            // means "try again next tick," not a fatal stream error —
            // matching the general engine-availability posture of this
            // server (a dead writer thread is an out-of-scope-for-v1
            // failure mode; see MEMORY.md).
            let Ok(xid) = state.engine.begin(None).await else {
                continue;
            };
            let result = state
                .engine
                .poll_events(xid, params.consumer.clone(), params.limit)
                .await;
            let _ = state.engine.commit(xid).await;

            let Ok(events) = result else {
                continue;
            };

            metrics::counter!("unidb_sse_poll_cycles_total").increment(1);
            if events.is_empty() {
                continue;
            }
            metrics::counter!("unidb_sse_events_delivered_total").increment(events.len() as u64);

            for event in events {
                let seq = event.seq;
                let op = event.op.clone();
                let payload = serde_json::to_string(&event).unwrap_or_else(|_| "{}".into());
                yield Ok(SseEvent::default().id(seq.to_string()).event(op).data(payload));
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}
