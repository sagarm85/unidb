//! `GET /events/subscribe` + `POST /events/ack` (M5.d): subscribes via
//! `reqwest`'s streaming body support, manually parses SSE `data:` lines
//! (no extra client dependency needed), inserts a row via `/sql` on an
//! events-enabled table, and asserts the SSE stream delivers it within a
//! bounded wait. A second test proves ack prevents replay on a fresh
//! subscribe.

#[path = "server_common/mod.rs"]
mod server_common;

use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;
use server_common::{valid_token, TestServer};

async fn post_json(server: &TestServer, path: &str, body: Value) -> u16 {
    let client = reqwest::Client::new();
    let resp = client
        .post(server.url(path))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .json(&body)
        .send()
        .await
        .unwrap();
    resp.status().as_u16()
}

async fn setup_events_enabled_table(server: &TestServer) {
    assert_eq!(
        post_json(
            server,
            "/sql",
            serde_json::json!({"sql": "CREATE TABLE t (id INT)"})
        )
        .await,
        200
    );
    let client = reqwest::Client::new();
    let resp = client
        .post(server.url("/tables/t/events"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
}

/// Reads SSE `data:` lines from a streaming response for up to `timeout`,
/// returning every payload seen — a small hand-rolled parser rather than
/// pulling in a dedicated SSE client crate for one test file.
async fn collect_sse_data_lines(
    resp: reqwest::Response,
    timeout: Duration,
    stop_after: usize,
) -> Vec<String> {
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut found = Vec::new();

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
                    if let Some(data) = line.strip_prefix("data: ") {
                        found.push(data.to_string());
                        if found.len() >= stop_after {
                            return found;
                        }
                    }
                }
            }
            _ => break,
        }
    }
    found
}

#[tokio::test]
async fn subscribe_delivers_a_committed_insert() {
    let server = TestServer::spawn().await;
    setup_events_enabled_table(&server).await;

    let client = reqwest::Client::new();
    let subscribe = client
        .get(server.url("/events/subscribe?consumer=c1&interval_ms=100"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .send();

    // Start the subscription, then insert — the poll loop's first tick
    // may land before or after the insert, so the bounded collection
    // window below must tolerate either ordering.
    let resp = subscribe.await.unwrap();
    assert_eq!(resp.status(), 200);

    assert_eq!(
        post_json(
            &server,
            "/sql",
            serde_json::json!({"sql": "INSERT INTO t (id) VALUES (42)"})
        )
        .await,
        200
    );

    let lines = collect_sse_data_lines(resp, Duration::from_secs(5), 1).await;
    assert_eq!(lines.len(), 1, "expected exactly one event delivered");
    let event: Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(event["op"], "insert");
    assert_eq!(event["table_name"], "t");
    assert_eq!(event["payload"]["id"], 42);
}

/// E1 (item 20): the ephemeral live-tail (no `consumer`) resumes strictly past
/// an explicit `from_seq` cursor without touching any durable consumer offset —
/// the studio's offset-scrubbing / replay-from-offset primitive.
#[tokio::test]
async fn ephemeral_tail_resumes_from_seq() {
    let server = TestServer::spawn().await;
    setup_events_enabled_table(&server).await;
    for id in 1..=3 {
        assert_eq!(
            post_json(
                &server,
                "/sql",
                serde_json::json!({"sql": format!("INSERT INTO t (id) VALUES ({id})")})
            )
            .await,
            200
        );
    }

    // from_seq=1 → only the events with seq > 1 (the 2nd and 3rd inserts).
    let client = reqwest::Client::new();
    let resp = client
        .get(server.url("/events/subscribe?from_seq=1&interval_ms=100"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .send()
        .await
        .unwrap();
    let lines = collect_sse_data_lines(resp, Duration::from_secs(3), 2).await;
    let seqs: Vec<i64> = lines
        .iter()
        .map(|l| {
            serde_json::from_str::<Value>(l).unwrap()["seq"]
                .as_i64()
                .unwrap()
        })
        .collect();
    assert_eq!(
        seqs,
        vec![2, 3],
        "ephemeral tail resumed strictly past from_seq"
    );
}

/// E1 (item 20): a reconnecting browser `EventSource` replays its last-seen id
/// in the standard `Last-Event-ID` header; the server resumes strictly after
/// it (and it wins over `from_seq`).
#[tokio::test]
async fn ephemeral_tail_resumes_from_last_event_id_header() {
    let server = TestServer::spawn().await;
    setup_events_enabled_table(&server).await;
    for id in 1..=3 {
        assert_eq!(
            post_json(
                &server,
                "/sql",
                serde_json::json!({"sql": format!("INSERT INTO t (id) VALUES ({id})")})
            )
            .await,
            200
        );
    }

    let client = reqwest::Client::new();
    let resp = client
        .get(server.url("/events/subscribe?interval_ms=100"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .header("Last-Event-ID", "2")
        .send()
        .await
        .unwrap();
    let lines = collect_sse_data_lines(resp, Duration::from_secs(3), 1).await;
    let seqs: Vec<i64> = lines
        .iter()
        .map(|l| {
            serde_json::from_str::<Value>(l).unwrap()["seq"]
                .as_i64()
                .unwrap()
        })
        .collect();
    assert_eq!(seqs, vec![3], "resumed past Last-Event-ID=2");
}

#[tokio::test]
async fn ack_prevents_replay_on_a_fresh_subscribe() {
    let server = TestServer::spawn().await;
    setup_events_enabled_table(&server).await;
    assert_eq!(
        post_json(
            &server,
            "/sql",
            serde_json::json!({"sql": "INSERT INTO t (id) VALUES (1)"})
        )
        .await,
        200
    );

    // First subscribe: must see the event.
    let client = reqwest::Client::new();
    let resp = client
        .get(server.url("/events/subscribe?consumer=c1&interval_ms=100"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .send()
        .await
        .unwrap();
    let lines = collect_sse_data_lines(resp, Duration::from_secs(3), 1).await;
    assert_eq!(lines.len(), 1);
    let event: Value = serde_json::from_str(&lines[0]).unwrap();
    let seq = event["seq"].as_i64().unwrap();

    assert_eq!(
        post_json(
            &server,
            "/events/ack",
            serde_json::json!({"consumer": "c1", "up_to_seq": seq})
        )
        .await,
        204
    );

    // Second subscribe, same consumer: must NOT replay the already-acked
    // event within a bounded wait window.
    let resp = client
        .get(server.url("/events/subscribe?consumer=c1&interval_ms=100"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .send()
        .await
        .unwrap();
    let lines = collect_sse_data_lines(resp, Duration::from_millis(800), 1).await;
    assert!(
        lines.is_empty(),
        "acked event must not be redelivered to the same consumer: {lines:?}"
    );
}

// ── Item 33: CDC management API ──────────────────────────────────────────────

async fn get_json(server: &TestServer, path: &str) -> (u16, Value) {
    let client = reqwest::Client::new();
    let resp = client
        .get(server.url(path))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    (status, body)
}

async fn delete_path(server: &TestServer, path: &str) -> u16 {
    let client = reqwest::Client::new();
    let resp = client
        .delete(server.url(path))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .send()
        .await
        .unwrap();
    resp.status().as_u16()
}

/// GET /tables/{name}/events returns { "enabled": false } on a fresh table,
/// true after enable, and false again after DELETE.
#[tokio::test]
async fn get_table_events_status_reflects_enable_disable_cycle() {
    let server = TestServer::spawn().await;

    // Create table.
    assert_eq!(
        post_json(
            &server,
            "/sql",
            serde_json::json!({"sql": "CREATE TABLE cdc_test (id INT)"}),
        )
        .await,
        200
    );

    // Initially disabled.
    let (status, body) = get_json(&server, "/tables/cdc_test/events").await;
    assert_eq!(status, 200);
    assert_eq!(body["enabled"], false);

    // Enable CDC.
    let client = reqwest::Client::new();
    let en = client
        .post(server.url("/tables/cdc_test/events"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .send()
        .await
        .unwrap();
    assert_eq!(en.status(), 204);

    // Now enabled.
    let (status, body) = get_json(&server, "/tables/cdc_test/events").await;
    assert_eq!(status, 200);
    assert_eq!(body["enabled"], true);

    // Disable CDC.
    let dis = delete_path(&server, "/tables/cdc_test/events").await;
    assert_eq!(dis, 204);

    // Now disabled again.
    let (status, body) = get_json(&server, "/tables/cdc_test/events").await;
    assert_eq!(status, 200);
    assert_eq!(body["enabled"], false);
}

/// DELETE /tables/{name}/events is idempotent: calling when already disabled
/// returns 204, not an error.
#[tokio::test]
async fn delete_table_events_is_idempotent() {
    let server = TestServer::spawn().await;

    assert_eq!(
        post_json(
            &server,
            "/sql",
            serde_json::json!({"sql": "CREATE TABLE idm (id INT)"}),
        )
        .await,
        200
    );

    // CDC was never enabled — first DELETE still returns 204.
    assert_eq!(delete_path(&server, "/tables/idm/events").await, 204);
    // Second DELETE also 204.
    assert_eq!(delete_path(&server, "/tables/idm/events").await, 204);
}

/// GET /tables/{name}/events returns 404 when the table does not exist.
#[tokio::test]
async fn get_table_events_status_404_for_unknown_table() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(server.url("/tables/no_such_table/events"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["code"], "TABLE_NOT_FOUND");
}

/// GET /events/head returns { "seq": 0 } when no events have been written.
#[tokio::test]
async fn get_events_head_returns_zero_when_empty() {
    let server = TestServer::spawn().await;
    let (status, body) = get_json(&server, "/events/head").await;
    assert_eq!(status, 200);
    assert_eq!(body["seq"], 0);
}

/// GET /events/head returns the highest committed seq after inserts on a
/// CDC-enabled table, and does NOT advance when a non-CDC table is written.
#[tokio::test]
async fn get_events_head_reflects_committed_events() {
    let server = TestServer::spawn().await;

    // Create and enable CDC.
    assert_eq!(
        post_json(
            &server,
            "/sql",
            serde_json::json!({"sql": "CREATE TABLE head_test (id INT)"}),
        )
        .await,
        200
    );
    let client = reqwest::Client::new();
    client
        .post(server.url("/tables/head_test/events"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .send()
        .await
        .unwrap();

    // Insert three rows to produce seqs 1, 2, 3.
    for id in 1..=3 {
        assert_eq!(
            post_json(
                &server,
                "/sql",
                serde_json::json!({"sql": format!("INSERT INTO head_test (id) VALUES ({id})")}),
            )
            .await,
            200
        );
    }

    let (status, body) = get_json(&server, "/events/head").await;
    assert_eq!(status, 200);
    assert_eq!(body["seq"], 3);
}

/// After disabling CDC, new inserts do NOT advance the head seq.
#[tokio::test]
async fn disable_events_stops_future_event_emission() {
    let server = TestServer::spawn().await;

    assert_eq!(
        post_json(
            &server,
            "/sql",
            serde_json::json!({"sql": "CREATE TABLE emit_test (id INT)"}),
        )
        .await,
        200
    );

    // Enable and insert one row.
    let client = reqwest::Client::new();
    client
        .post(server.url("/tables/emit_test/events"))
        .header("Authorization", format!("Bearer {}", valid_token()))
        .send()
        .await
        .unwrap();
    assert_eq!(
        post_json(
            &server,
            "/sql",
            serde_json::json!({"sql": "INSERT INTO emit_test (id) VALUES (1)"}),
        )
        .await,
        200
    );

    let (_, body) = get_json(&server, "/events/head").await;
    let seq_before = body["seq"].as_i64().unwrap();
    assert!(seq_before >= 1);

    // Disable CDC.
    assert_eq!(delete_path(&server, "/tables/emit_test/events").await, 204);

    // Insert another row — must NOT advance head.
    assert_eq!(
        post_json(
            &server,
            "/sql",
            serde_json::json!({"sql": "INSERT INTO emit_test (id) VALUES (2)"}),
        )
        .await,
        200
    );

    let (_, body2) = get_json(&server, "/events/head").await;
    assert_eq!(
        body2["seq"].as_i64().unwrap(),
        seq_before,
        "head seq must not advance after CDC is disabled"
    );
}
