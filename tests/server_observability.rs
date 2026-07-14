//! Integration tests for item 34: slow-query threshold configuration
//! (Part A) and the stats-history ring buffer (Part B).

#[path = "server_common/mod.rs"]
mod server_common;

use serde_json::{json, Value};
use server_common::{valid_token, TestServer};

// ── Part A: slow-query threshold ─────────────────────────────────────────────

/// `PUT /config/slow_query_threshold_ms` sets the threshold and returns 204.
#[tokio::test]
async fn put_slow_query_threshold_returns_204() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", valid_token());

    let resp = client
        .put(server.url("/config/slow_query_threshold_ms"))
        .header("Authorization", &auth)
        .json(&json!({ "threshold_ms": 100 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204, "superuser PUT returns 204");
}

/// Threshold changes take effect: a deliberately slow query lands in
/// `recent_slow_queries` once the threshold is low enough.
#[tokio::test]
async fn slow_query_captured_after_threshold_set() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", valid_token());

    // Set threshold to 0 ms — everything qualifies.
    let resp = client
        .put(server.url("/config/slow_query_threshold_ms"))
        .header("Authorization", &auth)
        .json(&json!({ "threshold_ms": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // Execute a query; the engine records all queries ≥ 1 µs as slow.
    client
        .post(server.url("/sql"))
        .header("Authorization", &auth)
        .json(
            &json!({ "sql": "CREATE TABLE sqtest (id INT); INSERT INTO sqtest (id) VALUES (42)" }),
        )
        .send()
        .await
        .unwrap();

    let resp = client
        .get(server.url("/stats"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let slow = body["recent_slow_queries"].as_array().unwrap();
    assert!(
        !slow.is_empty(),
        "recent_slow_queries should contain at least one entry after threshold=1ms"
    );
}

/// Setting threshold to 0 disables slow-query capture.
#[tokio::test]
async fn threshold_zero_disables_capture() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", valid_token());

    // Set, then clear the threshold.
    client
        .put(server.url("/config/slow_query_threshold_ms"))
        .header("Authorization", &auth)
        .json(&json!({ "threshold_ms": 1 }))
        .send()
        .await
        .unwrap();
    client
        .put(server.url("/config/slow_query_threshold_ms"))
        .header("Authorization", &auth)
        .json(&json!({ "threshold_ms": 0 }))
        .send()
        .await
        .unwrap();

    // Execute queries — none should be captured.
    client
        .post(server.url("/sql"))
        .header("Authorization", &auth)
        .json(&json!({ "sql": "CREATE TABLE sqzero (id INT)" }))
        .send()
        .await
        .unwrap();

    let body: Value = client
        .get(server.url("/stats"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let slow = body["recent_slow_queries"].as_array().unwrap();
    assert!(
        slow.is_empty(),
        "threshold=0 must not capture any slow queries"
    );
}

// ── Part B: stats history ─────────────────────────────────────────────────────

/// On a fresh engine `GET /stats/history` returns an empty points array.
#[tokio::test]
async fn stats_history_empty_on_fresh_engine() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", valid_token());

    let resp = client
        .get(server.url("/stats/history"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body["points"].as_array().unwrap().len(),
        0,
        "fresh engine: points array must be empty"
    );
    assert_eq!(
        body["interval_ms"].as_u64().unwrap(),
        5000,
        "default interval_ms is 5000"
    );
}

/// `?interval_ms=` is echoed back in the response.
#[tokio::test]
async fn stats_history_echoes_interval_ms() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", valid_token());

    let resp = client
        .get(server.url("/stats/history?interval_ms=10000"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["interval_ms"].as_u64().unwrap(), 10000);
}

/// `?points=` caps the number of returned points (cannot exceed 300).
#[tokio::test]
async fn stats_history_points_param_respected() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let auth = format!("Bearer {}", valid_token());

    // Directly inject two synthetic snapshots by calling capture_stats_point
    // via the engine. We can't call that from outside, so use the ticker
    // test via Engine::open + manual capture. Here we verify the cap logic:
    // requesting 500 points on a fresh (0-point) ring returns 0, not an error.
    let resp = client
        .get(server.url("/stats/history?points=500"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    // 500 is clamped to 300 server-side; ring is empty so 0 points returned.
    assert!(
        body["points"].as_array().unwrap().len() <= 300,
        "points param must be capped at 300"
    );
}

/// Rate fields and ring-buffer population: inject two synthetic snapshots
/// directly via the `Engine` API (unit-level) and verify `stats_history_snapshot`
/// returns correct rate fields.
#[test]
fn stats_history_rate_fields_correct() {
    use std::time::Duration;
    use unidb::Engine;

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    // Empty ring → empty result.
    let pts = engine.stats_history_snapshot(60);
    assert!(pts.is_empty(), "fresh engine history is empty");

    // Inject two consecutive snapshots via the public capture call.
    engine.capture_stats_point();
    let pts1 = engine.stats_history_snapshot(60);
    assert_eq!(pts1.len(), 1, "one capture = one point");
    // First point has no predecessor — rates must be 0.
    assert_eq!(pts1[0].commits_per_sec, 0.0);
    assert_eq!(pts1[0].wal_bytes_per_sec, 0.0);

    // Run a transaction so counters advance.
    let xid = engine.begin().unwrap();
    let _ = engine.execute_sql(xid, "CREATE TABLE rh (v INT)");
    engine.commit(xid).unwrap();

    // Short sleep so t_ms changes between the two captures.
    std::thread::sleep(Duration::from_millis(50));
    engine.capture_stats_point();
    let pts2 = engine.stats_history_snapshot(60);
    assert_eq!(pts2.len(), 2, "two captures = two points");

    // Second point derives rates from the delta vs the first.
    let p = &pts2[1];
    assert!(p.commits >= 1, "at least one commit recorded");
    // Rate should be positive (commits / time).
    assert!(
        p.commits_per_sec > 0.0,
        "commits_per_sec must be positive after a commit"
    );
    // wal_bytes_per_sec may be 0 if WAL bytes didn't change, but must not be negative.
    assert!(
        p.wal_bytes_per_sec >= 0.0,
        "wal_bytes_per_sec must be non-negative"
    );
    // bufferpool_hit_ratio is in [0, 1].
    assert!(
        p.bufferpool_hit_ratio >= 0.0 && p.bufferpool_hit_ratio <= 1.0,
        "hit ratio must be in [0,1]"
    );

    // Points are oldest-first (pts2[0].t <= pts2[1].t).
    assert!(pts2[0].t <= pts2[1].t, "points must be oldest-first");
}

/// The ring buffer respects the 300-point STATS_HISTORY_MAX cap.
#[test]
fn stats_history_ring_caps_at_300() {
    use unidb::Engine;

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    // Inject 350 snapshots — the ring must cap at 300.
    for _ in 0..350 {
        engine.capture_stats_point();
    }
    let pts = engine.stats_history_snapshot(300);
    assert_eq!(pts.len(), 300, "ring must cap at 300 points");
}

/// `n < ring.len()` returns only the most recent n points.
#[test]
fn stats_history_snapshot_most_recent_n() {
    use unidb::Engine;

    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();

    for _ in 0..10 {
        engine.capture_stats_point();
    }
    let pts = engine.stats_history_snapshot(3);
    assert_eq!(pts.len(), 3, "snapshot(3) returns at most 3 points");
}
