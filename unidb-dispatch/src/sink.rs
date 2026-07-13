//! Delivery targets for the dispatcher (item 20, E2). A [`Sink`] receives one
//! already-filtered, already-projected event at a time. Failures are the
//! dispatcher's concern, not the sink's: `deliver` returns `Err` and the
//! dispatcher applies the retry policy and, on exhaustion, dead-letters the
//! event (see `dlq.rs`). Sinks that cannot meaningfully fail (an in-process
//! room broadcast) simply never return `Err`.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use async_trait::async_trait;
use unidb::queue::Event;

/// A single delivery failure — the reason string is what lands in the
/// dead-letter table's `error` column.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SinkError(pub String);

impl SinkError {
    pub fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

#[async_trait]
pub trait Sink: Send + Sync {
    /// Stable name — used in logs and in the dead-letter table's `sink` column
    /// so an operator can tell which subscription failed.
    fn name(&self) -> &str;

    /// Deliver one event. `Err` triggers the dispatcher's retry + dead-letter
    /// path; the sink itself does no retrying.
    async fn deliver(&self, event: &Event) -> Result<(), SinkError>;
}

/// POST each event as JSON to an HTTP endpoint (E2b). One attempt per call —
/// retry/backoff and dead-lettering are the dispatcher's job. A non-2xx
/// response or a transport error is a failure.
pub struct WebhookSink {
    name: String,
    url: String,
    client: reqwest::Client,
}

impl WebhookSink {
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self::with_client(name, url, reqwest::Client::new())
    }

    /// Inject a pre-configured client (timeouts, headers, a test client, …).
    pub fn with_client(
        name: impl Into<String>,
        url: impl Into<String>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            client,
        }
    }
}

#[async_trait]
impl Sink for WebhookSink {
    fn name(&self) -> &str {
        &self.name
    }

    async fn deliver(&self, event: &Event) -> Result<(), SinkError> {
        let resp = self
            .client
            .post(&self.url)
            .json(event)
            .send()
            .await
            .map_err(|e| SinkError::new(format!("transport error: {e}")))?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(SinkError::new(format!("endpoint returned HTTP {status}")))
        }
    }
}

/// In-process fan-out room (E2a): every event is broadcast to all live
/// receivers. This is the primitive a studio backend's WebSocket/SSE room
/// layer subscribes to (E4, out of this repo) — the dispatcher owns the
/// engine-facing poll+ack loop; the room owns socket lifecycles. A broadcast
/// with no receivers is a no-op, not a failure: a live tail dropping frames
/// while nobody is watching is correct, so this sink never dead-letters.
pub struct RoomSink {
    name: String,
    tx: tokio::sync::broadcast::Sender<Event>,
    delivered: Arc<AtomicU64>,
}

impl RoomSink {
    pub fn new(name: impl Into<String>, capacity: usize) -> Self {
        let (tx, _rx) = tokio::sync::broadcast::channel(capacity.max(1));
        Self {
            name: name.into(),
            tx,
            delivered: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Subscribe a new receiver (one WebSocket/SSE client). Lagged receivers
    /// follow tokio's broadcast semantics: they observe `RecvError::Lagged`
    /// and skip ahead — the durable engine offset is untouched, so a slow
    /// *browser* never stalls the *durable* pipeline.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// Total events broadcast into the room (whether or not anyone was
    /// listening) — for observability/tests.
    pub fn delivered(&self) -> u64 {
        self.delivered.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl Sink for RoomSink {
    fn name(&self) -> &str {
        &self.name
    }

    async fn deliver(&self, event: &Event) -> Result<(), SinkError> {
        // `send` errors only when there are zero receivers; that is expected
        // and fine for a live room, so it is deliberately not a delivery
        // failure.
        let _ = self.tx.send(event.clone());
        self.delivered.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// Test/inspection sink: records every delivered event in memory. Also used as
/// a "downstream demo service" collector in the acceptance tests.
pub struct CollectingSink {
    name: String,
    events: std::sync::Mutex<Vec<Event>>,
}

impl CollectingSink {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            events: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn events(&self) -> Vec<Event> {
        self.events.lock().expect("collecting sink lock").clone()
    }

    pub fn seqs(&self) -> Vec<i64> {
        self.events().iter().map(|e| e.seq).collect()
    }
}

#[async_trait]
impl Sink for CollectingSink {
    fn name(&self) -> &str {
        &self.name
    }

    async fn deliver(&self, event: &Event) -> Result<(), SinkError> {
        self.events
            .lock()
            .expect("collecting sink lock")
            .push(event.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(seq: i64) -> Event {
        Event {
            seq,
            xid: 1,
            table_name: "t".into(),
            op: "insert".into(),
            payload: serde_json::json!({"id": seq}),
            before: None,
            after: None,
            ts_ms: 0,
        }
    }

    #[tokio::test]
    async fn collecting_sink_records() {
        let s = CollectingSink::new("c");
        s.deliver(&ev(1)).await.unwrap();
        s.deliver(&ev(2)).await.unwrap();
        assert_eq!(s.seqs(), vec![1, 2]);
    }

    #[tokio::test]
    async fn room_broadcasts_to_receivers_and_never_errors_without_them() {
        let room = RoomSink::new("room", 16);
        // No receivers yet → still Ok.
        room.deliver(&ev(1)).await.unwrap();

        let mut rx = room.subscribe();
        room.deliver(&ev(2)).await.unwrap();
        let got = rx.recv().await.unwrap();
        assert_eq!(got.seq, 2);
        assert_eq!(room.delivered(), 2);
    }
}
