#![cfg(feature = "server")]
//! Item 144 — Scheduled jobs (cron).
//!
//! Follows the `tests/item141_webhooks.rs` harness pattern closely — same
//! shape of problem (a superuser-registered admin object driving a
//! background worker that's a pure downstream caller of the engine). Uses
//! [`server_common::TestServer::spawn_with_cron`] to get the shared
//! `Arc<EngineHandle>`/`CronState` pair so tests drive the injectable
//! `unidb::server::cron::run_due(&engine, &cron, now)` directly with a
//! hand-picked timestamp — **no real wall-clock minute wait**. Because
//! `run_due` spawns each due job's execution without waiting for it (see
//! that function's module doc — a blocking `run_due` would defeat its own
//! no-overlap contract), tests observe completion the same way
//! `item141_webhooks.rs` observes async delivery: a bounded `wait_for` poll
//! of `GET /cron/jobs`.
//!
//! Test matrix (mirrors the backlog doc's acceptance list):
//!   (a) every_minute_job_inserts_marker_row_and_status_becomes_ok
//!   (b) bad_sql_job_records_error_while_second_healthy_job_still_runs
//!   (c) run_as_limited_role_is_subject_to_rls_parity_with_direct_execution
//!   (d) malformed_cron_schedule_returns_400
//!   (e) admin_routes_are_superuser_only
//!   (f) overlap_skip_when_previous_run_still_in_flight
//!   (g) disabled_job_never_runs
//!   (h) job_not_due_this_minute_is_skipped
//!   (i) delete_is_idempotent_and_removes_the_registration

use chrono::NaiveDate;
use serde_json::{json, Value};

#[path = "server_common/mod.rs"]
mod server_common;
use server_common::{token_for, valid_token, TestServer};

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

async fn sql(server: &TestServer, token: &str, sql_text: &str) -> (u16, Value) {
    let resp = client()
        .post(server.url("/sql"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({ "sql": sql_text }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

/// Bootstraps a `root` superuser — the standard cast for the admin-route
/// tests below (mirrors `item141_webhooks.rs::setup_root`).
async fn setup_root(server: &TestServer) -> String {
    let bootstrap = valid_token();
    assert_eq!(
        sql(server, &bootstrap, "CREATE USER root SUPERUSER")
            .await
            .0,
        200
    );
    token_for("root")
}

async fn register_job(server: &TestServer, token: &str, body: Value) -> (u16, Value) {
    let resp = client()
        .post(server.url("/cron/jobs"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = if status == 204 {
        Value::Null
    } else {
        resp.json().await.unwrap_or(Value::Null)
    };
    (status, body)
}

async fn list_jobs(server: &TestServer, token: &str) -> (u16, Value) {
    let resp = client()
        .get(server.url("/cron/jobs"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

async fn delete_job(server: &TestServer, token: &str, name: &str) -> u16 {
    client()
        .delete(server.url(&format!("/cron/jobs/{name}")))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

fn find_job<'a>(jobs: &'a Value, name: &str) -> Option<&'a Value> {
    jobs.as_array()?.iter().find(|j| j["name"] == name)
}

/// Poll `GET /cron/jobs` (every 20ms, up to 5s) until `name`'s `last_status`
/// is no longer `null` (i.e. the job has completed at least one run),
/// returning the final list. Mirrors `item141_webhooks.rs::wait_for`'s
/// bounded-poll shape, applied to the admin-status surface instead of a mock
/// HTTP receiver — `run_due` is deliberately non-blocking (see its module
/// doc), so a run's completion is only ever observable asynchronously.
async fn wait_for_completion(server: &TestServer, token: &str, name: &str) -> Value {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let (_, jobs) = list_jobs(server, token).await;
        if let Some(j) = find_job(&jobs, name) {
            if !j["last_status"].is_null() {
                return jobs;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return jobs;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// A fixed, arbitrary local timestamp used as `now` wherever the exact
/// calendar value doesn't matter (any `* * * * *` job matches it).
fn fixed_now() -> chrono::NaiveDateTime {
    NaiveDate::from_ymd_opt(2026, 6, 15)
        .unwrap()
        .and_hms_opt(10, 30, 0)
        .unwrap()
}

// ── (a) a `* * * * *` job inserts a marker row; status becomes ok ──────────

#[tokio::test]
async fn every_minute_job_inserts_marker_row_and_status_becomes_ok() {
    let (server, engine, cron) = TestServer::spawn_with_cron().await;
    let root = setup_root(&server).await;

    assert_eq!(
        sql(&server, &root, "CREATE TABLE markers (n INT)").await.0,
        200
    );

    let (status, _) = register_job(
        &server,
        &root,
        json!({
            "name": "insert-marker",
            "schedule": "* * * * *",
            "sql": "INSERT INTO markers (n) VALUES (1)",
        }),
    )
    .await;
    assert_eq!(status, 204);

    unidb::server::cron::run_due(&engine, &cron, fixed_now()).await;

    let jobs = wait_for_completion(&server, &root, "insert-marker").await;
    let job = find_job(&jobs, "insert-marker").expect("job present");
    assert_eq!(job["last_status"], "ok");
    assert!(job["last_error"].is_null());
    assert_eq!(job["run_count"], 1);
    assert!(job["last_run_at"].is_number());

    // The row really landed.
    let (status, body) = sql(&server, &root, "SELECT n FROM markers").await;
    assert_eq!(status, 200);
    let rows = body["results"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], 1);
}

// ── (b) a bad-SQL job records error; a second healthy job still runs ───────

#[tokio::test]
async fn bad_sql_job_records_error_while_second_healthy_job_still_runs() {
    let (server, engine, cron) = TestServer::spawn_with_cron().await;
    let root = setup_root(&server).await;

    assert_eq!(
        sql(&server, &root, "CREATE TABLE markers (n INT)").await.0,
        200
    );

    assert_eq!(
        register_job(
            &server,
            &root,
            json!({
                "name": "bad",
                "schedule": "* * * * *",
                "sql": "SELECT * FROM no_such_table_at_all",
            }),
        )
        .await
        .0,
        204
    );
    assert_eq!(
        register_job(
            &server,
            &root,
            json!({
                "name": "good",
                "schedule": "* * * * *",
                "sql": "INSERT INTO markers (n) VALUES (2)",
            }),
        )
        .await
        .0,
        204
    );

    unidb::server::cron::run_due(&engine, &cron, fixed_now()).await;

    let jobs = wait_for_completion(&server, &root, "bad").await;
    let bad = find_job(&jobs, "bad").expect("bad job present");
    assert_eq!(bad["last_status"], "error");
    assert!(bad["last_error"].is_string());
    assert!(!bad["last_error"].as_str().unwrap().is_empty());

    let jobs = wait_for_completion(&server, &root, "good").await;
    let good = find_job(&jobs, "good").expect("good job present");
    assert_eq!(
        good["last_status"], "ok",
        "a failing job must not block a healthy one"
    );
    assert_eq!(good["run_count"], 1);

    let (_, body) = sql(&server, &root, "SELECT n FROM markers").await;
    let rows = body["results"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], 2);
}

// ── (c) run_as a limited role → subject to that role's RLS/grants ──────────

#[tokio::test]
async fn run_as_limited_role_is_subject_to_rls_parity_with_direct_execution() {
    let (server, engine, cron) = TestServer::spawn_with_cron().await;
    let root = setup_root(&server).await;

    assert_eq!(
        sql(&server, &root, "CREATE TABLE posts (id INT, owner TEXT)")
            .await
            .0,
        200
    );
    assert_eq!(sql(&server, &root, "CREATE USER bob").await.0, 200);
    assert_eq!(
        sql(&server, &root, "GRANT INSERT, SELECT ON posts TO bob")
            .await
            .0,
        200
    );
    assert_eq!(
        sql(
            &server,
            &root,
            "CREATE POLICY p ON posts FOR INSERT USING (owner = current_user)"
        )
        .await
        .0,
        200
    );

    // Parity check #1: running the exact same SQL directly as bob (via
    // /sql) is rejected by the policy — establishes what "as bob" means
    // before comparing the cron job's outcome against it.
    let bob = token_for("bob");
    let (direct_status, direct_body) = sql(
        &server,
        &bob,
        "INSERT INTO posts (id, owner) VALUES (99, 'mallory')",
    )
    .await;
    assert_ne!(
        direct_status, 200,
        "direct execution as bob must be rejected by the policy"
    );
    assert!(direct_body["error"]
        .as_str()
        .unwrap_or_default()
        .contains("violates policy"));

    // A job scoped to bob, inserting a row it doesn't own — must be
    // rejected exactly like the direct call above.
    assert_eq!(
        register_job(
            &server,
            &root,
            json!({
                "name": "bob-violating",
                "schedule": "* * * * *",
                "sql": "INSERT INTO posts (id, owner) VALUES (1, 'mallory')",
                "run_as": "bob",
            }),
        )
        .await
        .0,
        204
    );
    // A job scoped to bob, inserting a row it DOES own — must succeed,
    // exactly like `execute_sql_as(Some("bob"), ..)` would.
    assert_eq!(
        register_job(
            &server,
            &root,
            json!({
                "name": "bob-owned",
                "schedule": "* * * * *",
                "sql": "INSERT INTO posts (id, owner) VALUES (2, 'bob')",
                "run_as": "bob",
            }),
        )
        .await
        .0,
        204
    );

    unidb::server::cron::run_due(&engine, &cron, fixed_now()).await;

    let jobs = wait_for_completion(&server, &root, "bob-violating").await;
    let violating = find_job(&jobs, "bob-violating").unwrap();
    assert_eq!(violating["last_status"], "error");
    assert!(violating["last_error"]
        .as_str()
        .unwrap_or_default()
        .contains("violates policy"));

    let jobs = wait_for_completion(&server, &root, "bob-owned").await;
    let owned = find_job(&jobs, "bob-owned").unwrap();
    assert_eq!(owned["last_status"], "ok");

    // Only bob's own row made it in.
    let (_, body) = sql(&server, &root, "SELECT id, owner FROM posts ORDER BY id").await;
    let rows = body["results"][0]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], "bob");
}

// ── (d) malformed cron schedule → 400 ───────────────────────────────────────

#[tokio::test]
async fn malformed_cron_schedule_returns_400() {
    let (server, _engine, _cron) = TestServer::spawn_with_cron().await;
    let root = setup_root(&server).await;

    for bad_schedule in ["not a cron", "* * * *", "60 * * * *", "* * * * 8"] {
        let (status, body) = register_job(
            &server,
            &root,
            json!({
                "name": "bad-schedule",
                "schedule": bad_schedule,
                "sql": "SELECT 1",
            }),
        )
        .await;
        assert_eq!(status, 400, "schedule '{bad_schedule}' must be rejected");
        assert_eq!(body["code"], "INVALID_CRON_SCHEDULE");
    }

    // A well-formed registration after the rejected attempts still works —
    // the failed validations must not have left any partial state behind.
    assert_eq!(
        register_job(
            &server,
            &root,
            json!({
                "name": "good-schedule",
                "schedule": "*/5 * * * *",
                "sql": "SELECT 1",
            }),
        )
        .await
        .0,
        204
    );
}

// ── (e) admin routes are superuser-only ─────────────────────────────────────

#[tokio::test]
async fn admin_routes_are_superuser_only() {
    let (server, _engine, _cron) = TestServer::spawn_with_cron().await;
    let root = setup_root(&server).await;
    assert_eq!(sql(&server, &root, "CREATE USER alice").await.0, 200);
    let alice = token_for("alice");

    let (status, _) = register_job(
        &server,
        &alice,
        json!({"name": "x", "schedule": "* * * * *", "sql": "SELECT 1"}),
    )
    .await;
    assert_eq!(status, 403);

    let (status, _) = list_jobs(&server, &alice).await;
    assert_eq!(status, 403);

    assert_eq!(delete_job(&server, &alice, "x").await, 403);
}

// ── (f) overlap: a job whose previous run is in flight is skipped ──────────

#[tokio::test]
async fn overlap_skip_when_previous_run_still_in_flight() {
    let (server, engine, cron) = TestServer::spawn_with_cron().await;
    let root = setup_root(&server).await;

    assert_eq!(
        sql(&server, &root, "CREATE TABLE markers (n INT)").await.0,
        200
    );
    assert_eq!(
        register_job(
            &server,
            &root,
            json!({
                "name": "slow",
                "schedule": "* * * * *",
                "sql": "INSERT INTO markers (n) VALUES (3)",
            }),
        )
        .await
        .0,
        204
    );

    // Deterministically simulate "previous run still in flight" by driving
    // the exact same no-overlap guard `run_due` itself uses — see
    // `CronState::try_start`'s doc comment for why this is the
    // race-free way to exercise this path (a real concurrent-tick race
    // would be timing-dependent and flaky).
    assert!(cron.try_start("slow"));

    unidb::server::cron::run_due(&engine, &cron, fixed_now()).await;

    // Give any (incorrectly) spawned task a moment to land, then assert it
    // did NOT run: no row inserted, no status recorded by `run_due` itself
    // (the in-flight `try_start` we hold is still active — a `finish` here
    // would only ever come from a run `run_due` actually started).
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let (_, jobs) = list_jobs(&server, &root).await;
    let job = find_job(&jobs, "slow").unwrap();
    assert_eq!(
        job["run_count"], 0,
        "an in-flight job must not stack a second run"
    );
    assert!(job["last_status"].is_null());

    let (_, body) = sql(&server, &root, "SELECT n FROM markers").await;
    let rows = body["results"][0]["rows"].as_array().unwrap();
    assert!(rows.is_empty());

    // Release the guard and confirm the job runs normally on the next tick.
    cron.finish("slow", Ok(()));
    unidb::server::cron::run_due(&engine, &cron, fixed_now()).await;
    let jobs = wait_for_completion(&server, &root, "slow").await;
    let job = find_job(&jobs, "slow").unwrap();
    assert_eq!(job["last_status"], "ok");
    assert_eq!(job["run_count"], 1);
}

// ── (g) a disabled job never runs ───────────────────────────────────────────

#[tokio::test]
async fn disabled_job_never_runs() {
    let (server, engine, cron) = TestServer::spawn_with_cron().await;
    let root = setup_root(&server).await;

    assert_eq!(
        sql(&server, &root, "CREATE TABLE markers (n INT)").await.0,
        200
    );
    assert_eq!(
        register_job(
            &server,
            &root,
            json!({
                "name": "off",
                "schedule": "* * * * *",
                "sql": "INSERT INTO markers (n) VALUES (4)",
                "enabled": false,
            }),
        )
        .await
        .0,
        204
    );

    unidb::server::cron::run_due(&engine, &cron, fixed_now()).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let (_, jobs) = list_jobs(&server, &root).await;
    let job = find_job(&jobs, "off").unwrap();
    assert_eq!(job["run_count"], 0);
    assert!(job["last_status"].is_null());
}

// ── (h) a job not due this minute is skipped ────────────────────────────────

#[tokio::test]
async fn job_not_due_this_minute_is_skipped() {
    let (server, engine, cron) = TestServer::spawn_with_cron().await;
    let root = setup_root(&server).await;

    assert_eq!(
        sql(&server, &root, "CREATE TABLE markers (n INT)").await.0,
        200
    );
    // Fires only at exactly midnight on Jan 1st.
    assert_eq!(
        register_job(
            &server,
            &root,
            json!({
                "name": "yearly",
                "schedule": "0 0 1 1 *",
                "sql": "INSERT INTO markers (n) VALUES (5)",
            }),
        )
        .await
        .0,
        204
    );

    // `fixed_now` is nowhere near midnight on Jan 1st.
    unidb::server::cron::run_due(&engine, &cron, fixed_now()).await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let (_, jobs) = list_jobs(&server, &root).await;
    let job = find_job(&jobs, "yearly").unwrap();
    assert_eq!(job["run_count"], 0);
    assert!(job["last_status"].is_null());

    // Now drive it at exactly the matching instant — it runs.
    let matching = NaiveDate::from_ymd_opt(2027, 1, 1)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    unidb::server::cron::run_due(&engine, &cron, matching).await;
    let jobs = wait_for_completion(&server, &root, "yearly").await;
    let job = find_job(&jobs, "yearly").unwrap();
    assert_eq!(job["last_status"], "ok");
}

// ── (i) delete is idempotent and removes the registration ──────────────────

#[tokio::test]
async fn delete_is_idempotent_and_removes_the_registration() {
    let (server, engine, cron) = TestServer::spawn_with_cron().await;
    let root = setup_root(&server).await;

    assert_eq!(
        register_job(
            &server,
            &root,
            json!({"name": "temp", "schedule": "* * * * *", "sql": "SELECT 1"}),
        )
        .await
        .0,
        204
    );
    let (_, jobs) = list_jobs(&server, &root).await;
    assert!(find_job(&jobs, "temp").is_some());

    assert_eq!(delete_job(&server, &root, "temp").await, 204);
    let (_, jobs) = list_jobs(&server, &root).await;
    assert!(find_job(&jobs, "temp").is_none());

    // Idempotent: deleting an unknown name is still 204, not an error.
    assert_eq!(delete_job(&server, &root, "temp").await, 204);
    assert_eq!(delete_job(&server, &root, "never-existed").await, 204);

    // A deleted job no longer runs even if `run_due` is driven again.
    unidb::server::cron::run_due(&engine, &cron, fixed_now()).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let (_, jobs) = list_jobs(&server, &root).await;
    assert!(find_job(&jobs, "temp").is_none());
}
