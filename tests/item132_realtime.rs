#![cfg(feature = "server")]
//! Item 132: Realtime Broadcast + Presence integration tests.
//!
//! Follows the harness pattern from `tests/server_events.rs`
//! (`TestServer`/`valid_token` from `tests/server_common`, a hand-rolled SSE
//! line parser rather than pulling in a dedicated SSE client crate).
//! `collect_sse_frames` below extends `server_events.rs`'s
//! `collect_sse_data_lines` to also capture the `event:` line, since
//! presence frames are told apart by their `event` (`sync`/`join`/`leave`/
//! `update`), not just their `data`.

#[path = "server_common/mod.rs"]
mod server_common;

use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;
use server_common::{token_for, valid_token, TestServer};

async fn subscribe(server: &TestServer, path: &str, token: &str) -> reqwest::Response {
    let client = reqwest::Client::new();
    client
        .get(server.url(path))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap()
}

/// Reads SSE frames (`event:`/`data:` line pairs, separated by the blank
/// line SSE frames terminate on) from a streaming response for up to
/// `timeout`, stopping early once `stop_after` frames are collected.
async fn collect_sse_frames(
    resp: reqwest::Response,
    timeout: Duration,
    stop_after: usize,
) -> Vec<(String, String)> {
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut found = Vec::new();
    let mut cur_event: Option<String> = None;
    let mut cur_data: Option<String> = None;

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() || found.len() >= stop_after {
            break;
        }
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                buf.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(pos) = buf.find('\n') {
                    let line = buf[..pos].trim_end_matches('\r').to_string();
                    buf.drain(..=pos);
                    if line.is_empty() {
                        if let (Some(ev), Some(data)) = (cur_event.take(), cur_data.take()) {
                            found.push((ev, data));
                            if found.len() >= stop_after {
                                return found;
                            }
                        }
                    } else if let Some(ev) = line.strip_prefix("event: ") {
                        cur_event = Some(ev.to_string());
                    } else if let Some(data) = line.strip_prefix("data: ") {
                        cur_data = Some(data.to_string());
                    }
                }
            }
            _ => break,
        }
    }
    found
}

// ── Broadcast ──────────────────────────────────────────────────────────

/// Two subscribers on the same topic both receive a published broadcast; a
/// subscriber on a *different* topic does not (spec's isolation AC).
#[tokio::test]
async fn broadcast_delivers_to_topic_subscribers_and_isolates_other_topics() {
    let server = TestServer::spawn().await;
    let token = valid_token();

    // `get_broadcast_subscribe` registers the subscription before
    // returning headers (see its doc comment), so by the time each
    // `.await` below resolves, the subscription is already live — no
    // sleep/retry needed to avoid a lost-publish race.
    let resp_a = subscribe(&server, "/realtime/broadcast/subscribe?topic=t1", &token).await;
    let resp_b = subscribe(&server, "/realtime/broadcast/subscribe?topic=t1", &token).await;
    let resp_c = subscribe(&server, "/realtime/broadcast/subscribe?topic=t2", &token).await;
    assert_eq!(resp_a.status(), 200);
    assert_eq!(resp_b.status(), 200);
    assert_eq!(resp_c.status(), 200);

    let client = reqwest::Client::new();
    let publish_resp = client
        .post(server.url("/realtime/broadcast/publish"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "topic": "t1",
            "event": "msg",
            "payload": {"hello": "world"}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(publish_resp.status(), 200);
    let publish_body: Value = publish_resp.json().await.unwrap();
    assert_eq!(
        publish_body["receivers"], 2,
        "both t1 subscribers must be counted as receivers"
    );

    let frames_a = collect_sse_frames(resp_a, Duration::from_secs(3), 1).await;
    let frames_b = collect_sse_frames(resp_b, Duration::from_secs(3), 1).await;
    let frames_c = collect_sse_frames(resp_c, Duration::from_millis(800), 1).await;

    assert_eq!(frames_a.len(), 1, "subscriber A must receive the broadcast");
    assert_eq!(frames_a[0].0, "msg");
    let payload_a: Value = serde_json::from_str(&frames_a[0].1).unwrap();
    assert_eq!(payload_a["topic"], "t1");
    assert_eq!(payload_a["event"], "msg");
    assert_eq!(payload_a["payload"]["hello"], "world");
    assert!(payload_a["ts"].is_i64());

    assert_eq!(frames_b.len(), 1, "subscriber B must receive the broadcast");
    let payload_b: Value = serde_json::from_str(&frames_b[0].1).unwrap();
    assert_eq!(payload_b["payload"]["hello"], "world");

    assert!(
        frames_c.is_empty(),
        "a subscriber on a different topic must not receive the broadcast: {frames_c:?}"
    );
}

// ── Presence ───────────────────────────────────────────────────────────

/// A tracked presence key appears in a late subscriber's initial `sync`
/// frame, and closing the tracking connection emits `leave` to the others
/// (spec's presence AC).
#[tokio::test]
async fn presence_sync_includes_tracked_key_and_disconnect_emits_leave() {
    let server = TestServer::spawn().await;
    let alice_token = token_for("alice");
    let bob_token = token_for("bob");

    // `get_presence_subscribe` registers the connection (before returning
    // headers) so the subsequent `track` call below is guaranteed to
    // attribute the key to this now-live connection.
    let resp_alice = subscribe(
        &server,
        "/realtime/presence/subscribe?topic=p1",
        &alice_token,
    )
    .await;
    assert_eq!(resp_alice.status(), 200);

    // Alice's own initial sync frame is empty (nothing tracked yet).
    let alice_frames = collect_sse_frames(resp_alice, Duration::from_secs(3), 1).await;
    assert_eq!(alice_frames.len(), 1);
    assert_eq!(alice_frames[0].0, "sync");
    let alice_sync: Value = serde_json::from_str(&alice_frames[0].1).unwrap();
    assert_eq!(alice_sync["payload"], serde_json::json!({}));

    // Re-subscribe (Alice's connection above was fully consumed by
    // collect_sse_frames' single-frame stop, which also completes the
    // response and lets the connection close — we need a still-open
    // connection to later observe its `leave` effect). Keep this second
    // stream open and don't fully consume it yet.
    let resp_alice_live = subscribe(
        &server,
        "/realtime/presence/subscribe?topic=p1",
        &alice_token,
    )
    .await;
    assert_eq!(resp_alice_live.status(), 200);

    let client = reqwest::Client::new();
    let track_resp = client
        .post(server.url("/realtime/presence/track"))
        .header("Authorization", format!("Bearer {alice_token}"))
        .json(&serde_json::json!({
            "topic": "p1",
            "key": "alice",
            "state": {"status": "online"}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(track_resp.status(), 204);

    // Late subscriber: its initial `sync` frame must already include the
    // key Alice tracked.
    let resp_bob = subscribe(&server, "/realtime/presence/subscribe?topic=p1", &bob_token).await;
    assert_eq!(resp_bob.status(), 200);

    let bob_frames = collect_sse_frames(resp_bob, Duration::from_secs(3), 1).await;
    assert_eq!(bob_frames.len(), 1);
    assert_eq!(bob_frames[0].0, "sync");
    let bob_sync: Value = serde_json::from_str(&bob_frames[0].1).unwrap();
    assert_eq!(
        bob_sync["payload"]["alice"],
        serde_json::json!({"status": "online"}),
        "late subscriber's sync must include the previously tracked key: {bob_sync}"
    );

    // Open one more presence watcher and keep it open across the drop
    // below: its own initial `sync` frame arrives first (registration is
    // guaranteed complete before this `.await` resolves — see
    // `get_presence_subscribe`'s doc comment), then dropping Alice's live
    // tracking connection must push a `leave` delta as the *next* frame on
    // this stream. Run the drop concurrently with the collection so the
    // collector is already listening when the delta is broadcast.
    let resp_bob_watch =
        subscribe(&server, "/realtime/presence/subscribe?topic=p1", &bob_token).await;
    assert_eq!(resp_bob_watch.status(), 200);
    let watch_task = tokio::spawn(collect_sse_frames(
        resp_bob_watch,
        Duration::from_secs(3),
        2,
    ));
    // Drop Alice's still-open tracking connection: the drop guard removes
    // her held key and broadcasts `leave` to the topic's other subscribers.
    drop(resp_alice_live);
    let frames = watch_task.await.unwrap();

    assert_eq!(frames.len(), 2, "expected sync then leave: {frames:?}");
    assert_eq!(frames[0].0, "sync");
    assert_eq!(frames[1].0, "leave");
    let leave_payload: Value = serde_json::from_str(&frames[1].1).unwrap();
    assert_eq!(leave_payload["event"], "leave");
    assert_eq!(leave_payload["payload"]["key"], "alice");

    // Belt-and-suspenders: a brand new subscriber's sync should now show
    // the key is gone too.
    let resp_after = subscribe(&server, "/realtime/presence/subscribe?topic=p1", &bob_token).await;
    let after_frames = collect_sse_frames(resp_after, Duration::from_secs(3), 1).await;
    assert_eq!(after_frames.len(), 1);
    let after_sync: Value = serde_json::from_str(&after_frames[0].1).unwrap();
    assert_eq!(
        after_sync["payload"],
        serde_json::json!({}),
        "key must be gone from presence after its tracking connection closed: {after_sync}"
    );
}

/// A presence key tracked by one caller is invisible on an unrelated topic
/// (mirrors the broadcast isolation test — presence state is per topic).
#[tokio::test]
async fn presence_isolated_between_topics() {
    let server = TestServer::spawn().await;
    let token = valid_token();

    let resp_live = subscribe(&server, "/realtime/presence/subscribe?topic=q1", &token).await;
    assert_eq!(resp_live.status(), 200);

    let client = reqwest::Client::new();
    let track_resp = client
        .post(server.url("/realtime/presence/track"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({"topic": "q1", "key": "u1", "state": {"x": 1}}))
        .send()
        .await
        .unwrap();
    assert_eq!(track_resp.status(), 204);

    let resp_other_topic =
        subscribe(&server, "/realtime/presence/subscribe?topic=q2", &token).await;
    let frames = collect_sse_frames(resp_other_topic, Duration::from_secs(2), 1).await;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].0, "sync");
    let sync: Value = serde_json::from_str(&frames[0].1).unwrap();
    assert_eq!(
        sync["payload"],
        serde_json::json!({}),
        "a different topic's presence must not see q1's tracked key"
    );

    drop(resp_live);
}
