//! Acceptance (item 20, E2b): a webhook fan-out to a failing endpoint retries,
//! then dead-letters the event into a table **inside unidb** (dogfood), while
//! the pipeline keeps advancing (a poison event cannot wedge the stream).

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use unidb::sql::executor::ExecResult;
use unidb::sql::logical::Literal;
use unidb::Engine;
use unidb_dispatch::{Dispatcher, Filter, RetryPolicy, WebhookSink};

/// A minimal HTTP endpoint that always answers `500`, counting every request.
/// Returns its base URL and the shared hit counter.
async fn spawn_failing_endpoint() -> (String, Arc<AtomicU64>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicU64::new(0));
    let hits_task = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            hits_task.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 500 Internal Server Error\r\n\
                          Content-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (format!("http://{addr}/hook"), hits)
}

fn commit_sql(engine: &Engine, sql: &str) {
    let xid = engine.begin().unwrap();
    engine.execute_sql(xid, sql).unwrap();
    engine.commit(xid).unwrap();
}

/// Read every row of a table as `ExecResult::Rows`.
fn select_rows(engine: &Engine, table: &str) -> (Vec<String>, Vec<Vec<Literal>>) {
    let xid = engine.begin().unwrap();
    let results = engine
        .execute_sql(xid, &format!("SELECT * FROM {table}"))
        .unwrap();
    engine.commit(xid).unwrap();
    match results.into_iter().next() {
        Some(ExecResult::Rows { columns, rows }) => (columns, rows),
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failing_webhook_retries_then_dead_letters() {
    let (url, hits) = spawn_failing_endpoint().await;

    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::open(dir.path(), 0).unwrap());
    commit_sql(&engine, "CREATE TABLE t (id INT)");
    engine.enable_events("t").unwrap();
    commit_sql(&engine, "INSERT INTO t (id) VALUES (99)");

    let webhook = Arc::new(WebhookSink::new("orders-hook", url));
    let dispatcher = Dispatcher::builder(engine.clone(), "webhook-consumer")
        .subscribe(Filter::table("t"), webhook)
        .retry(RetryPolicy {
            max_attempts: 3,
            base_backoff: Duration::from_millis(5),
        })
        .dlq_table("dispatch_dead_letter")
        .build();

    let report = dispatcher.run_once().await.unwrap();

    // Retried the configured number of times before giving up.
    assert_eq!(
        hits.load(Ordering::SeqCst),
        3,
        "endpoint hit once per attempt"
    );
    assert_eq!(report.dead_lettered, 1, "the event was dead-lettered");
    assert_eq!(report.delivered, 0);
    assert_eq!(dispatcher.stats().dead_lettered.load(Ordering::Relaxed), 1);

    // The pipeline still advanced: a poison event does not wedge the stream.
    assert_eq!(
        report.acked_up_to,
        Some(1),
        "offset advanced past the poison event"
    );

    // The dead letter is durably in unidb (dogfood), with the failure context.
    let (columns, rows) = select_rows(&engine, "dispatch_dead_letter");
    assert_eq!(rows.len(), 1, "exactly one dead-letter row");
    let idx = |name: &str| columns.iter().position(|c| c == name).unwrap();
    let row = &rows[0];
    assert_eq!(row[idx("seq")], Literal::Int(1));
    assert_eq!(row[idx("table_name")], Literal::Text("t".into()));
    assert_eq!(row[idx("op")], Literal::Text("insert".into()));
    assert_eq!(row[idx("sink")], Literal::Text("orders-hook".into()));
    assert_eq!(row[idx("attempts")], Literal::Int(3));
    match &row[idx("error")] {
        Literal::Text(e) => assert!(e.contains("500"), "error records the HTTP status: {e}"),
        other => panic!("error column should be text, got {other:?}"),
    }
    // Payload round-trips as JSON with the original row image.
    match &row[idx("payload")] {
        Literal::Json(p) => {
            let v: serde_json::Value = serde_json::from_str(p).unwrap();
            assert_eq!(v["id"], serde_json::json!(99));
        }
        other => panic!("payload column should be json, got {other:?}"),
    }
}
