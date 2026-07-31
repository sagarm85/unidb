//! Item 124 (Workstream E1): per-subscriber RLS/grant authorization on
//! `GET /events/subscribe`. Before E1 the SSE stream was JWT-gated but
//! delivered every event for a table to any authenticated subscriber; these
//! tests prove the delivery-side filter closes that gap end to end, over the
//! real HTTP route (not just the underlying `Engine` method), mirroring the
//! embedded-engine tenant-isolation acceptance test in
//! `tests/item122_auth_uid_jwt.rs::auth_jwt_tenant_isolation_two_tokens`.

#![cfg(feature = "server")]

#[path = "server_common/mod.rs"]
mod server_common;

use std::time::Duration;

use futures_util::StreamExt;
use serde_json::{json, Value};
use server_common::{token_for, token_with_claims, valid_token, TestServer};

async fn sql(server: &TestServer, token: &str, sql: &str) -> (u16, Value) {
    let client = reqwest::Client::new();
    let resp = client
        .post(server.url("/sql"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({ "sql": sql }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap();
    (status, body)
}

async fn enable_events(server: &TestServer, token: &str, table: &str) {
    let client = reqwest::Client::new();
    let resp = client
        .post(server.url(&format!("/tables/{table}/events")))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
}

/// Reads SSE `data:` lines from a streaming response for up to `timeout`,
/// stopping early once `stop_after` lines have been seen. Mirrors
/// `tests/server_events.rs::collect_sse_data_lines` exactly (kept as its own
/// copy — a shared `tests/server_common` helper would need every
/// `server_*.rs` file that doesn't stream SSE to tolerate an unused-fn
/// warning under `-D warnings`).
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

async fn subscribe(server: &TestServer, token: &str, query: &str) -> reqwest::Response {
    reqwest::Client::new()
        .get(server.url(&format!("/events/subscribe?{query}")))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap()
}

/// Bootstraps: `root` (superuser), a `tenant_id`-policy-scoped table with
/// events enabled, `svc` granted table-level SELECT on it.
async fn setup_tenant_scoped_table(server: &TestServer, table: &str) -> String {
    let admin = valid_token();
    assert_eq!(
        sql(server, &admin, "CREATE USER root SUPERUSER").await.0,
        200
    );
    let root = token_for("root");
    assert_eq!(
        sql(
            server,
            &root,
            &format!("CREATE TABLE {table} (id INT, tenant_id TEXT, val TEXT)"),
        )
        .await
        .0,
        200
    );
    enable_events(server, &root, table).await;
    assert_eq!(sql(server, &root, "CREATE USER svc").await.0, 200);
    assert_eq!(
        sql(server, &root, &format!("GRANT SELECT ON {table} TO svc"),)
            .await
            .0,
        200
    );
    let (policy_status, policy_body) = sql(
        server,
        &root,
        &format!(
            "CREATE POLICY p ON {table} FOR SELECT USING (tenant_id = (auth.jwt() ->> 'tenant'))"
        ),
    )
    .await;
    assert_eq!(policy_status, 200, "{policy_body}");
    root
}

// ── (a) two tenants, one shared table, isolated realtime streams ───────────

#[tokio::test]
async fn two_tenants_receive_only_their_own_tenants_events() {
    let server = TestServer::spawn().await;
    let table = "rt_tenants";
    let root = setup_tenant_scoped_table(&server, table).await;

    let acme_token = token_with_claims(Some("svc"), &[("tenant", json!("acme"))]);
    let globex_token = token_with_claims(Some("svc"), &[("tenant", json!("globex"))]);

    let acme_sub = subscribe(
        &server,
        &acme_token,
        &format!("table={table}&interval_ms=100"),
    )
    .await;
    assert_eq!(acme_sub.status(), 200);
    let globex_sub = subscribe(
        &server,
        &globex_token,
        &format!("table={table}&interval_ms=100"),
    )
    .await;
    assert_eq!(globex_sub.status(), 200);

    // One shared table, one shared INSERT statement — the row-image RLS
    // filter, not a query-time WHERE clause, must do the isolating.
    assert_eq!(
        sql(
            &server,
            &root,
            &format!(
                "INSERT INTO {table} (id, tenant_id, val) VALUES \
                 (1, 'acme', 'a1'), (2, 'globex', 'g1'), (3, 'acme', 'a2')"
            ),
        )
        .await
        .0,
        200
    );

    let acme_lines = collect_sse_data_lines(acme_sub, Duration::from_secs(5), 2).await;
    let globex_lines = collect_sse_data_lines(globex_sub, Duration::from_secs(5), 1).await;

    assert_eq!(
        acme_lines.len(),
        2,
        "acme subscriber should see exactly its 2 rows, got {acme_lines:?}"
    );
    for line in &acme_lines {
        let event: Value = serde_json::from_str(line).unwrap();
        assert_eq!(event["payload"]["tenant_id"], "acme", "{event}");
    }

    assert_eq!(
        globex_lines.len(),
        1,
        "globex subscriber should see exactly its 1 row, got {globex_lines:?}"
    );
    let globex_event: Value = serde_json::from_str(&globex_lines[0]).unwrap();
    assert_eq!(globex_event["payload"]["tenant_id"], "globex");
}

// ── (b) table-grant gate: no SELECT grant -> 403 at subscribe time ─────────

#[tokio::test]
async fn subscriber_without_select_grant_is_rejected_with_403() {
    let server = TestServer::spawn().await;
    let admin = valid_token();
    assert_eq!(
        sql(&server, &admin, "CREATE USER root SUPERUSER").await.0,
        200
    );
    let root = token_for("root");
    assert_eq!(
        sql(&server, &root, "CREATE TABLE priv_t (id INT)").await.0,
        200
    );
    assert_eq!(sql(&server, &root, "CREATE USER bob").await.0, 200);
    // Deliberately NO grant to bob.

    let bob = token_for("bob");
    let resp = subscribe(&server, &bob, "table=priv_t").await;
    assert_eq!(
        resp.status(),
        403,
        "a subscriber with no SELECT grant on the table must be rejected at subscribe time"
    );
}

// ── (c) service_role bypasses RLS/grants entirely, and is audited ──────────

#[tokio::test]
async fn service_role_receives_every_tenants_events_and_is_audited() {
    let server = TestServer::spawn().await;
    let table = "rt_service_role";
    let root = setup_tenant_scoped_table(&server, table).await;

    // A service_role token carries no matching `tenant` claim at all — under
    // the tenant policy alone this would see 0 rows; the bypass must still
    // deliver all of them.
    let service_role_token = token_with_claims(None, &[("role", json!("service_role"))]);

    let sub = subscribe(
        &server,
        &service_role_token,
        &format!("table={table}&interval_ms=100"),
    )
    .await;
    assert_eq!(sub.status(), 200);

    assert_eq!(
        sql(
            &server,
            &root,
            &format!(
                "INSERT INTO {table} (id, tenant_id, val) VALUES (1, 'acme', 'a1'), (2, 'globex', 'g1')"
            ),
        )
        .await
        .0,
        200
    );

    let lines = collect_sse_data_lines(sub, Duration::from_secs(5), 2).await;
    assert_eq!(
        lines.len(),
        2,
        "service_role subscriber must receive every tenant's events, got {lines:?}"
    );

    // Audited distinctly from the unaudited embedded/superuser bypass
    // (item-103 lesson) — the same `service_role_rls_bypass` action the
    // query path already logs.
    let audit = std::fs::read_to_string(server.data_dir().join("audit.log")).unwrap();
    assert!(
        audit
            .lines()
            .any(|l| l.contains("\"action\":\"service_role_rls_bypass\"")
                && l.contains("\"object\":\"events/subscribe\"")),
        "service_role realtime bypass must be audited; audit.log:\n{audit}"
    );
}

// ── (d) DELETE events filter on the BEFORE image, not AFTER (which is null) ─

#[tokio::test]
async fn delete_events_filter_on_before_image() {
    let server = TestServer::spawn().await;
    let table = "rt_delete_before";
    let root = setup_tenant_scoped_table(&server, table).await;

    let acme_token = token_with_claims(Some("svc"), &[("tenant", json!("acme"))]);
    let sub = subscribe(
        &server,
        &acme_token,
        &format!("table={table}&interval_ms=100"),
    )
    .await;
    assert_eq!(sub.status(), 200);

    assert_eq!(
        sql(
            &server,
            &root,
            &format!(
                "INSERT INTO {table} (id, tenant_id, val) VALUES (1, 'acme', 'a1'), (2, 'globex', 'g1')"
            ),
        )
        .await
        .0,
        200
    );
    assert_eq!(
        sql(&server, &root, &format!("DELETE FROM {table} WHERE id = 1"))
            .await
            .0,
        200,
        "delete acme's row"
    );
    assert_eq!(
        sql(&server, &root, &format!("DELETE FROM {table} WHERE id = 2"))
            .await
            .0,
        200,
        "delete globex's row"
    );

    // Expected for the acme subscriber: INSERT(acme) + DELETE(acme) only —
    // globex's INSERT and DELETE are both filtered out. If the DELETE filter
    // wrongly consulted the (always-null) AFTER image instead of BEFORE,
    // acme's own DELETE would ALSO be dropped (fail-closed on a null image),
    // so seeing it here proves BEFORE is what's actually being evaluated.
    let lines = collect_sse_data_lines(sub, Duration::from_secs(5), 2).await;
    assert_eq!(
        lines.len(),
        2,
        "acme subscriber should see its own INSERT + DELETE only, got {lines:?}"
    );

    let events: Vec<Value> = lines
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(events.iter().any(|e| e["op"] == "insert"), "{events:?}");
    let delete_event = events
        .iter()
        .find(|e| e["op"] == "delete")
        .expect("expected a delete event for acme's own row");
    assert_eq!(delete_event["payload"]["tenant_id"], "acme");
    assert_eq!(delete_event["payload"]["id"], 1);
    assert!(
        delete_event["after"].is_null(),
        "DELETE has no AFTER image: {delete_event}"
    );
}
