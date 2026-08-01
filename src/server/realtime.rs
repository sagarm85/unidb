//! `POST /realtime/broadcast/publish`, `GET /realtime/broadcast/subscribe`,
//! `GET /realtime/presence/subscribe`, `POST /realtime/presence/track`
//! (item 132: Supabase-parity Realtime Broadcast + Presence).
//!
//! **Purely in-memory and ephemeral — never touches the engine.** Neither
//! broadcast messages nor presence state are written to the WAL, buffer
//! pool, heap, or catalog; nothing is persisted, and a server restart drops
//! all state (same semantics as Supabase). This is why the crash-injection
//! harness and the storage engine's ACID guarantees are unaffected *by
//! construction* — no storage code path changes for this item. The only
//! shared state is the in-memory [`RealtimeState`] registry below, guarded
//! by a `std::sync::Mutex`, entirely off the commit/write path.
//!
//! ## Transport
//! Same SSE shape as `src/server/sse.rs` (axum `Sse` + `KeepAlive` +
//! `async_stream::stream!`) — no WebSocket transport, per spec.
//!
//! ## Broadcast
//! One `tokio::sync::broadcast::Sender<BroadcastFrame>` per topic, created
//! lazily on first subscribe and reclaimed (best-effort) when its last
//! receiver drops. Publish is fan-out, at-most-once, best-effort: a lagged
//! receiver silently drops frames (documented, not backpressure onto the
//! publisher) and a topic with zero current subscribers silently discards
//! (the publish response's `receivers` count tells the caller that).
//!
//! ## Presence
//! Registry: `topic -> key -> (state, holder connection ids)`. A presence
//! **connection** is one open `GET /realtime/presence/subscribe` SSE
//! stream, identified internally by a monotonic id and the caller's JWT
//! `sub` (empty string for an unauthenticated-subject/superuser caller).
//!
//! `POST /realtime/presence/track` has no connection id in its wire format
//! (matching the spec's documented request body exactly:
//! `{topic, key, state}`), so v1 attributes a tracked key to **every
//! currently-open presence/subscribe connection belonging to the same
//! caller (JWT `sub`) on that topic** — i.e. "this identity is present on
//! this topic for as long as at least one of its presence/subscribe
//! connections for the topic stays open." A key's `leave` fires once the
//! *last* holding connection closes (the drop-guard below), giving the
//! "two tabs of the same key both count" ref-counted behavior the spec
//! asks for while staying within the documented 4-route wire format. A
//! `track` call made with no live presence/subscribe connection yet open
//! for that (topic, identity) creates an orphaned entry with no holder —
//! documented v1 gap: it persists until a matching connection later opens
//! and closes, not indefinitely dangerous (still bounded, still ephemeral,
//! still wiped on restart) but not auto-reaped either. Flagged here rather
//! than silently accepted.
//!
//! ## Channel authorization (item 140)
//! Every route below now runs a **connect-time / publish-time** gate through
//! [`crate::server::engine_handle::EngineHandle::check_channel_authz`] before
//! doing anything else: `POST /realtime/broadcast/publish` and `POST
//! /realtime/presence/track` are gated before the message is fanned out /
//! the key is tracked; `GET /realtime/broadcast/subscribe` and `GET
//! /realtime/presence/subscribe` are gated before the SSE stream is opened
//! (mirrors `src/server/sse.rs`'s E1 subscribe-time grant gate — a clean
//! `403 PERMISSION_DENIED` beats a silently-empty stream). A denial never
//! reaches the in-memory hub at all.
//!
//! The decision itself — role-based, topic-glob channel policies,
//! `UNIDB_REALTIME_REQUIRE_AUTHZ`'s opt-in fail-closed posture, and the
//! audited `service_role`/superuser bypass — lives in [`crate::Engine::
//! check_channel_authz`] and [`crate::authz::RoleStore::
//! check_channel_policy`]; see those doc comments and `docs/REST_API.md` for
//! the full contract. **Not re-litigated here**: this module only calls the
//! gate and maps a denial to `403`.
//!
//! **Connect-time only, not retroactive (locked v1 design):** a policy
//! change takes effect on the next `subscribe`/`publish`/`track` call, but
//! never tears down a stream that already passed its gate — identical to
//! `sse.rs`'s E1 contract, and explicitly a v1 non-goal per the item-140
//! backlog doc.
//!
//! ## Never log payloads or presence state
//! Every `tracing` call in this module logs topic names/counts only, never
//! `payload`/`state` JSON values — both may carry application data.

use std::{
    collections::{HashMap, HashSet},
    convert::Infallible,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        IntoResponse, Response,
    },
    Extension, Json,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast::{self, error::RecvError};

use crate::{authz::ChannelOp, server::error::ApiError, server::AppState, AuthPrincipal};

/// Per-topic channel capacity for both broadcast and presence-delta
/// channels. A slow subscriber whose receiver falls this many frames behind
/// the publisher gets `RecvError::Lagged` (frames silently dropped for that
/// subscriber, no effect on others or on the publisher) rather than
/// unbounded memory growth.
const CHANNEL_CAPACITY: usize = 256;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ── Wire frames ─────────────────────────────────────────────────────────

/// `GET /realtime/broadcast/subscribe` SSE frame body (also the shape
/// fanned out internally).
#[derive(Debug, Clone, Serialize)]
pub struct BroadcastFrame {
    pub topic: String,
    pub event: String,
    pub payload: Value,
    pub ts: i64,
}

/// `GET /realtime/presence/subscribe` SSE frame body. `event` is one of
/// `"sync" | "join" | "leave" | "update"`. `payload` is the full
/// `{key: state, ...}` map for `sync`, or `{"key": ..., "state": ...}` for
/// `join`/`update` (`state` omitted for `leave`, which carries only `key`).
#[derive(Debug, Clone, Serialize)]
pub struct PresenceFrame {
    pub topic: String,
    pub event: String,
    pub payload: Value,
    pub ts: i64,
}

#[derive(Debug, Deserialize)]
pub struct BroadcastPublishRequest {
    pub topic: String,
    pub event: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Serialize)]
pub struct BroadcastPublishResponse {
    /// Number of current subscribers the message was fanned out to
    /// (best-effort, at-most-once — see the module doc).
    pub receivers: usize,
}

#[derive(Debug, Deserialize)]
pub struct PresenceTrackRequest {
    pub topic: String,
    pub key: String,
    #[serde(default)]
    pub state: Value,
}

#[derive(Debug, Deserialize)]
pub struct TopicParams {
    pub topic: String,
}

// ── In-memory hub ───────────────────────────────────────────────────────

struct PresenceKeyState {
    state: Value,
    /// Connection ids currently "holding" (keeping present) this key — see
    /// the module doc's ref-count model.
    holders: HashSet<u64>,
}

#[derive(Default)]
struct RealtimeInner {
    broadcast: HashMap<String, broadcast::Sender<BroadcastFrame>>,
    presence_channels: HashMap<String, broadcast::Sender<PresenceFrame>>,
    /// topic -> key -> state
    presence: HashMap<String, HashMap<String, PresenceKeyState>>,
    /// (topic, identity) -> live presence/subscribe connection ids.
    identity_conns: HashMap<(String, String), HashSet<u64>>,
    /// conn_id -> (topic, identity, keys this connection currently holds).
    conn_meta: HashMap<u64, (String, String, HashSet<String>)>,
}

/// The realtime hub held (behind an `Arc`) on `AppState`. Cheap to clone the
/// `Arc`; the hub itself is one `Mutex`-guarded map set — see the module doc
/// for the data-shape rationale.
pub struct RealtimeState {
    inner: Mutex<RealtimeInner>,
    next_conn_id: AtomicU64,
}

impl Default for RealtimeState {
    fn default() -> Self {
        Self::new()
    }
}

impl RealtimeState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(RealtimeInner::default()),
            next_conn_id: AtomicU64::new(1),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RealtimeInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ── Broadcast ──────────────────────────────────────────────────────

    /// Fan `event`/`payload` out to every current subscriber of `topic`.
    /// Returns the receiver count at send time (0 if the topic has no
    /// subscriber right now — the message is simply dropped, no topic
    /// pre-registration required).
    pub fn publish_broadcast(&self, topic: &str, event: String, payload: Value) -> usize {
        let inner = self.lock();
        let Some(tx) = inner.broadcast.get(topic) else {
            return 0;
        };
        let receivers = tx.receiver_count();
        let frame = BroadcastFrame {
            topic: topic.to_string(),
            event,
            payload,
            ts: now_ms(),
        };
        // `send` only errors when there are zero receivers, which the
        // `receiver_count()` snapshot above already reflects — a benign
        // race (a receiver drops between the count and the send) just
        // means this frame reaches one fewer subscriber, which is exactly
        // the documented best-effort/at-most-once contract.
        let _ = tx.send(frame);
        receivers
    }

    /// Subscribe to `topic`'s broadcast channel, creating it if this is the
    /// first subscriber.
    fn subscribe_broadcast(&self, topic: &str) -> broadcast::Receiver<BroadcastFrame> {
        let mut inner = self.lock();
        inner
            .broadcast
            .entry(topic.to_string())
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .subscribe()
    }

    /// Best-effort reclaim: drop `topic`'s sender once no receiver remains.
    /// Safe to call from a `Drop` impl — `receiver_count()` counts every
    /// live receiver clone including ones not yet dropped, so `<= 1`
    /// (rather than `== 0`) is robust regardless of whether this fires
    /// before or after the calling connection's own receiver is dropped.
    fn reclaim_broadcast(&self, topic: &str) {
        let mut inner = self.lock();
        if let Some(tx) = inner.broadcast.get(topic) {
            if tx.receiver_count() <= 1 {
                inner.broadcast.remove(topic);
            }
        }
    }

    // ── Presence ───────────────────────────────────────────────────────

    /// Register a new presence/subscribe connection for `(topic, identity)`
    /// and return its id.
    fn open_presence_conn(&self, topic: &str, identity: &str) -> u64 {
        let conn_id = self.next_conn_id.fetch_add(1, Ordering::Relaxed);
        let mut inner = self.lock();
        inner
            .identity_conns
            .entry((topic.to_string(), identity.to_string()))
            .or_default()
            .insert(conn_id);
        inner.conn_meta.insert(
            conn_id,
            (topic.to_string(), identity.to_string(), HashSet::new()),
        );
        conn_id
    }

    /// Full current presence map for `topic` as `{key: state, ...}` — the
    /// initial `sync` frame body.
    fn presence_snapshot(&self, topic: &str) -> Value {
        let inner = self.lock();
        let mut obj = serde_json::Map::new();
        if let Some(map) = inner.presence.get(topic) {
            for (key, entry) in map {
                obj.insert(key.clone(), entry.state.clone());
            }
        }
        Value::Object(obj)
    }

    fn subscribe_presence(&self, topic: &str) -> broadcast::Receiver<PresenceFrame> {
        let mut inner = self.lock();
        inner
            .presence_channels
            .entry(topic.to_string())
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .subscribe()
    }

    fn send_presence_frame(&self, topic: &str, event: &str, payload: Value) {
        let tx = {
            let mut inner = self.lock();
            inner
                .presence_channels
                .entry(topic.to_string())
                .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
                .clone()
        };
        let frame = PresenceFrame {
            topic: topic.to_string(),
            event: event.to_string(),
            payload,
            ts: now_ms(),
        };
        let _ = tx.send(frame);
    }

    /// `POST /realtime/presence/track`: upsert `key`'s state under `topic`,
    /// attributing it to every currently-open presence/subscribe connection
    /// for `(topic, identity)` (see module doc), and push `join` (new key)
    /// or `update` (existing key) to the topic's presence subscribers.
    pub fn track(&self, topic: &str, identity: &str, key: &str, state: Value) {
        let is_join = {
            let mut inner = self.lock();
            let conn_ids: Vec<u64> = inner
                .identity_conns
                .get(&(topic.to_string(), identity.to_string()))
                .map(|s| s.iter().copied().collect())
                .unwrap_or_default();
            let topic_map = inner.presence.entry(topic.to_string()).or_default();
            let is_join = !topic_map.contains_key(key);
            let entry = topic_map
                .entry(key.to_string())
                .or_insert_with(|| PresenceKeyState {
                    state: Value::Null,
                    holders: HashSet::new(),
                });
            entry.state = state.clone();
            for conn_id in &conn_ids {
                entry.holders.insert(*conn_id);
            }
            for conn_id in &conn_ids {
                if let Some((_, _, keys)) = inner.conn_meta.get_mut(conn_id) {
                    keys.insert(key.to_string());
                }
            }
            is_join
        };
        let event = if is_join { "join" } else { "update" };
        self.send_presence_frame(
            topic,
            event,
            serde_json::json!({"key": key, "state": state}),
        );
    }

    /// Drop-guard callback: release everything `conn_id` was holding on
    /// `topic`, broadcasting `leave` for any key whose last holder this
    /// was, then reclaim the presence channel if it has no subscriber left.
    fn close_presence_conn(&self, topic: &str, conn_id: u64) {
        let mut leaves = Vec::new();
        {
            let mut inner = self.lock();
            if let Some((_, identity, keys)) = inner.conn_meta.remove(&conn_id) {
                if let Some(set) = inner
                    .identity_conns
                    .get_mut(&(topic.to_string(), identity.clone()))
                {
                    set.remove(&conn_id);
                    if set.is_empty() {
                        inner.identity_conns.remove(&(topic.to_string(), identity));
                    }
                }
                if let Some(topic_map) = inner.presence.get_mut(topic) {
                    for key in keys {
                        if let Some(entry) = topic_map.get_mut(&key) {
                            entry.holders.remove(&conn_id);
                            if entry.holders.is_empty() {
                                topic_map.remove(&key);
                                leaves.push(key);
                            }
                        }
                    }
                    if topic_map.is_empty() {
                        inner.presence.remove(topic);
                    }
                }
            }
            if let Some(tx) = inner.presence_channels.get(topic) {
                if tx.receiver_count() <= 1 {
                    inner.presence_channels.remove(topic);
                }
            }
        }
        for key in leaves {
            self.send_presence_frame(topic, "leave", serde_json::json!({"key": key}));
        }
    }
}

/// Drops with the broadcast SSE stream — reclaims the topic's sender once
/// this was the last subscriber.
struct BroadcastConnGuard {
    realtime: Arc<RealtimeState>,
    topic: String,
}

impl Drop for BroadcastConnGuard {
    fn drop(&mut self) {
        self.realtime.reclaim_broadcast(&self.topic);
    }
}

/// Drops with the presence SSE stream — this *is* the connection's presence
/// membership (module doc): releases every key it held and broadcasts
/// `leave` for any that had no other holder.
struct PresenceConnGuard {
    realtime: Arc<RealtimeState>,
    topic: String,
    conn_id: u64,
}

impl Drop for PresenceConnGuard {
    fn drop(&mut self) {
        self.realtime.close_presence_conn(&self.topic, self.conn_id);
    }
}

// ── Handlers ────────────────────────────────────────────────────────────

/// `POST /realtime/broadcast/publish`.
pub async fn post_broadcast_publish(
    State(state): State<AppState>,
    Extension(principal): Extension<AuthPrincipal>,
    Json(body): Json<BroadcastPublishRequest>,
) -> Result<Json<BroadcastPublishResponse>, ApiError> {
    state
        .engine
        .check_channel_authz(
            principal,
            body.topic.clone(),
            ChannelOp::Publish,
            "realtime/broadcast/publish",
        )
        .await?;
    tracing::debug!(topic = %body.topic, event = %body.event, "realtime broadcast publish");
    let receivers = state
        .realtime
        .publish_broadcast(&body.topic, body.event, body.payload);
    Ok(Json(BroadcastPublishResponse { receivers }))
}

/// `GET /realtime/broadcast/subscribe?topic=<t>`.
///
/// Registers the subscription (creates the topic's channel if needed and
/// takes a receiver) **before** returning the response, not inside the SSE
/// body stream — so a publish that races the caller's very next request is
/// never lost to "the subscription hadn't registered yet" (the same
/// register-before-headers guarantee `get_presence_subscribe` relies on).
pub async fn get_broadcast_subscribe(
    State(state): State<AppState>,
    Extension(principal): Extension<AuthPrincipal>,
    Query(params): Query<TopicParams>,
) -> Result<Response, ApiError> {
    let topic = params.topic;
    // item 140: connect-time gate, before the subscription is ever
    // registered — a denial must never reach the in-memory hub.
    state
        .engine
        .check_channel_authz(
            principal,
            topic.clone(),
            ChannelOp::Subscribe,
            "realtime/broadcast/subscribe",
        )
        .await?;
    let realtime = state.realtime.clone();
    tracing::debug!(topic = %topic, "realtime broadcast subscribe");
    let mut rx = realtime.subscribe_broadcast(&topic);

    let stream = async_stream::stream! {
        let _guard = BroadcastConnGuard { realtime: realtime.clone(), topic: topic.clone() };
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    let event = frame.event.clone();
                    let data = serde_json::to_string(&frame).unwrap_or_default();
                    yield Ok::<_, Infallible>(SseEvent::default().event(event).data(data));
                }
                Err(RecvError::Lagged(n)) => {
                    // Documented drop, not backpressure onto the publisher
                    // (module doc) — never logs payloads.
                    tracing::debug!(topic = %topic, dropped = n, "broadcast subscriber lagged");
                }
                Err(RecvError::Closed) => break,
            }
        }
    };

    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}

/// `GET /realtime/presence/subscribe?topic=<t>`.
pub async fn get_presence_subscribe(
    State(state): State<AppState>,
    Extension(principal): Extension<AuthPrincipal>,
    Query(params): Query<TopicParams>,
) -> Result<Response, ApiError> {
    let topic = params.topic;
    // item 140: connect-time gate, before the connection is registered. Both
    // presence routes (this one and `POST .../track`) share `ChannelOp::
    // Presence` — see the module doc's `ChannelOp` note.
    state
        .engine
        .check_channel_authz(
            principal.clone(),
            topic.clone(),
            ChannelOp::Presence,
            "realtime/presence/subscribe",
        )
        .await?;
    let identity = principal.subject.clone().unwrap_or_default();
    let realtime = state.realtime.clone();
    tracing::debug!(topic = %topic, "realtime presence subscribe");

    // Register the connection, subscribe to presence deltas, and take the
    // snapshot — all before returning the response (headers go out only
    // after this), same register-before-headers guarantee
    // `get_broadcast_subscribe` uses: a `track`/disconnect racing in right
    // after this call is either reflected in the snapshot or delivered as
    // a delta afterward, never lost.
    let conn_id = realtime.open_presence_conn(&topic, &identity);
    let mut rx = realtime.subscribe_presence(&topic);
    let snapshot = realtime.presence_snapshot(&topic);

    let stream = async_stream::stream! {
        let _guard = PresenceConnGuard { realtime: realtime.clone(), topic: topic.clone(), conn_id };

        let sync_frame = PresenceFrame {
            topic: topic.clone(),
            event: "sync".to_string(),
            payload: snapshot,
            ts: now_ms(),
        };
        yield Ok::<_, Infallible>(
            SseEvent::default()
                .event("sync")
                .data(serde_json::to_string(&sync_frame).unwrap_or_default()),
        );

        loop {
            match rx.recv().await {
                Ok(frame) => {
                    let event = frame.event.clone();
                    let data = serde_json::to_string(&frame).unwrap_or_default();
                    yield Ok::<_, Infallible>(SseEvent::default().event(event).data(data));
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::debug!(topic = %topic, dropped = n, "presence subscriber lagged");
                }
                Err(RecvError::Closed) => break,
            }
        }
    };

    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}

/// `POST /realtime/presence/track`.
pub async fn post_presence_track(
    State(state): State<AppState>,
    Extension(principal): Extension<AuthPrincipal>,
    Json(body): Json<PresenceTrackRequest>,
) -> Result<StatusCode, ApiError> {
    state
        .engine
        .check_channel_authz(
            principal.clone(),
            body.topic.clone(),
            ChannelOp::Presence,
            "realtime/presence/track",
        )
        .await?;
    let identity = principal.subject.clone().unwrap_or_default();
    tracing::debug!(topic = %body.topic, key = %body.key, "realtime presence track");
    state
        .realtime
        .track(&body.topic, &identity, &body.key, body.state);
    Ok(StatusCode::NO_CONTENT)
}
