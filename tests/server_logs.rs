//! Item 22, L3 — `GET /logs` over HTTP: superuser gate, bounded cursor
//! pagination, filters, and the L2 `x-request-id` / audit correlation the
//! endpoint is meant to serve.

#[path = "server_common/mod.rs"]
mod server_common;

use std::fs::File;
use std::io::Write;
use std::path::Path;

use serde_json::{json, Value};
use server_common::{token_for, valid_token, TestServer};

async fn sql(client: &reqwest::Client, url: String, token: &str, sql: &str) -> reqwest::Response {
    client
        .post(url)
        .header("Authorization", format!("Bearer {token}"))
        .json(&json!({ "sql": sql }))
        .send()
        .await
        .unwrap()
}

async fn get_logs(
    client: &reqwest::Client,
    server: &TestServer,
    token: &str,
    query: &str,
) -> reqwest::Response {
    client
        .get(server.url(&format!("/logs{query}")))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap()
}

/// Drop a synthetic rotated JSON log file into the server's log dir.
fn write_log_file(log_dir: &Path, name: &str, lines: &[String]) {
    let mut f = File::create(log_dir.join(name)).unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}

fn log_line(ts: &str, level: &str, msg: &str, request_id: &str) -> String {
    json!({
        "timestamp": ts,
        "level": level,
        "target": "unidb::test",
        "fields": { "message": msg },
        "request_id": request_id,
    })
    .to_string()
}

#[tokio::test]
async fn logs_endpoint_is_superuser_gated() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let admin = valid_token(); // open-mode superuser until users exist

    // Register a SUPERUSER + a plain user so open-mode no longer applies.
    assert_eq!(
        sql(
            &client,
            server.url("/sql"),
            &admin,
            "CREATE USER root SUPERUSER"
        )
        .await
        .status(),
        200
    );
    let root = token_for("root");
    assert_eq!(
        sql(&client, server.url("/sql"), &root, "CREATE USER bob")
            .await
            .status(),
        200
    );

    // Superuser: allowed. Named non-superuser: 403.
    assert_eq!(get_logs(&client, &server, &root, "").await.status(), 200);
    let bob = token_for("bob");
    assert_eq!(get_logs(&client, &server, &bob, "").await.status(), 403);

    // No token at all → 401 from the auth layer (never reaches the gate).
    assert_eq!(
        client
            .get(server.url("/logs"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
}

#[tokio::test]
async fn logs_are_bounded_cursor_paged_and_filterable() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let root = valid_token(); // open mode → superuser

    // 25 lines, newest last; one carries a distinctive request_id for `q`.
    let mut lines: Vec<String> = (0..25)
        .map(|i| {
            log_line(
                &format!("2026-07-13T00:00:{i:02}Z"),
                if i % 5 == 0 { "WARN" } else { "INFO" },
                &format!("line {i}"),
                if i == 7 {
                    "req-needle-xyz"
                } else {
                    "req-other"
                },
            )
        })
        .collect();
    // Interleave a second (older) rotated file to exercise cross-file paging.
    let older: Vec<String> = (0..5)
        .map(|i| {
            log_line(
                &format!("2026-07-12T00:00:{i:02}Z"),
                "INFO",
                "old",
                "req-old",
            )
        })
        .collect();
    write_log_file(server.log_dir(), "unidb.log.2026-07-13", &lines);
    write_log_file(server.log_dir(), "unidb.log.2026-07-12", &older);
    lines.extend(older);

    // Page through in bounded steps and gather every message once.
    let mut cursor: Option<String> = None;
    let mut collected: Vec<String> = Vec::new();
    for _ in 0..20 {
        let q = match &cursor {
            Some(c) => format!("?limit=10&cursor={c}"),
            None => "?limit=10".to_string(),
        };
        let body: Value = get_logs(&client, &server, &root, &q)
            .await
            .json()
            .await
            .unwrap();
        let logs = body["logs"].as_array().unwrap();
        assert!(logs.len() <= 10, "page never exceeds the requested limit");
        for l in logs {
            collected.push(l["fields"]["message"].as_str().unwrap().to_string());
        }
        match body["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => {
                cursor = None;
                break;
            }
        }
    }
    assert!(cursor.is_none(), "pagination terminates");
    assert_eq!(collected.len(), 30, "all 30 lines returned exactly once");
    // Newest-first: the 2026-07-13 line 24 comes before any 2026-07-12 "old".
    assert_eq!(collected.first().unwrap(), "line 24");
    assert_eq!(collected.last().unwrap(), "old");

    // Substring filter narrows to the single needle line.
    let body: Value = get_logs(&client, &server, &root, "?q=req-needle-xyz")
        .await
        .json()
        .await
        .unwrap();
    let logs = body["logs"].as_array().unwrap();
    assert_eq!(logs.len(), 1);
    assert_eq!(logs[0]["fields"]["message"].as_str().unwrap(), "line 7");

    // Level filter: only WARN-and-above (lines 0,5,10,15,20 → 5 of them).
    let body: Value = get_logs(&client, &server, &root, "?level=WARN")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["logs"].as_array().unwrap().len(), 5);

    // Hard cap: even limit=100000 is clamped to MAX_PAGE (well above our 30).
    let body: Value = get_logs(&client, &server, &root, "?limit=100000")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(body["logs"].as_array().unwrap().len(), 30);
}

#[tokio::test]
async fn request_id_flows_to_response_header_and_audit_log() {
    let server = TestServer::spawn().await;
    let client = reqwest::Client::new();
    let admin = valid_token();

    // Bootstrap a superuser + table + user so we can run an audited GRANT.
    assert_eq!(
        sql(
            &client,
            server.url("/sql"),
            &admin,
            "CREATE USER root SUPERUSER"
        )
        .await
        .status(),
        200
    );
    let root = token_for("root");
    for stmt in ["CREATE TABLE t (id INT)", "CREATE USER bob"] {
        assert_eq!(
            sql(&client, server.url("/sql"), &root, stmt).await.status(),
            200
        );
    }

    // An audited auth-DDL statement (GRANT → execute_sql_as → audit.log).
    let resp = sql(
        &client,
        server.url("/sql"),
        &root,
        "GRANT SELECT ON t TO bob",
    )
    .await;
    assert_eq!(resp.status(), 200);

    // L2: the request id is echoed back to the client...
    let request_id = resp
        .headers()
        .get("x-request-id")
        .expect("x-request-id header present")
        .to_str()
        .unwrap()
        .to_string();
    assert!(request_id.starts_with("req-"), "got {request_id}");

    // ...and the same id lands in the durable audit.log, joining the HTTP
    // request to its security-trail entry (L2 correlation, end to end).
    let audit = std::fs::read_to_string(server.data_dir().join("audit.log")).unwrap();
    let grant_line = audit
        .lines()
        .find(|l| l.contains("\"action\":\"grant\""))
        .expect("grant is audited");
    assert!(
        grant_line.contains(&request_id),
        "audit line must carry the request_id {request_id}: {grant_line}"
    );
    assert!(grant_line.contains("\"txn_id\":"), "and the txn_id");
}
