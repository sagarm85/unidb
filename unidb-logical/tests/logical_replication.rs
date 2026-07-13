// Integration tests for logical replication (item 28, R2).
//
// These tests drive the full `LogicalReplicator` stack: primary commits trigger
// events, the replicator applies them to a target engine, and we verify the
// target converges to the same state. The primary restart test validates the
// at-least-once, offset-durable contract.

use std::sync::Arc;

use tempfile::tempdir;
use unidb::Engine;
use unidb_logical::{LogicalReplicator, TableSpec};

fn count(engine: &Engine, sql: &str) -> usize {
    let xid = engine.begin().unwrap();
    let rows = engine.execute_sql(xid, sql).unwrap();
    engine.commit(xid).unwrap();
    match &rows[0] {
        unidb::sql::executor::ExecResult::Rows { rows: r, .. } => r.len(),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[tokio::test]
async fn inserts_are_applied_to_target() {
    let src = tempdir().unwrap();
    let tgt = tempdir().unwrap();

    // Primary: create table, enable events, insert rows.
    let primary = Arc::new(Engine::open(src.path(), 0).unwrap());
    {
        let xid = primary.begin().unwrap();
        primary
            .execute_sql(xid, "CREATE TABLE items (id INT, name TEXT)")
            .unwrap();
        primary.commit(xid).unwrap();
    }
    primary.enable_events("items").unwrap();
    for i in 1..=3 {
        let xid = primary.begin().unwrap();
        primary
            .execute_sql(
                xid,
                &format!("INSERT INTO items (id, name) VALUES ({i}, 'item{i}')"),
            )
            .unwrap();
        primary.commit(xid).unwrap();
    }

    // Target: same schema, no rows yet.
    let target = Arc::new(Engine::open(tgt.path(), 0).unwrap());
    {
        let xid = target.begin().unwrap();
        target
            .execute_sql(xid, "CREATE TABLE items (id INT, name TEXT)")
            .unwrap();
        target.commit(xid).unwrap();
    }
    assert_eq!(count(&target, "SELECT id FROM items"), 0);

    // Run logical replicator once.
    let replicator = LogicalReplicator::builder(
        primary.clone(),
        target.clone(),
        "lr-test-consumer",
        vec![TableSpec {
            table: "items".to_string(),
            key_column: "id".to_string(),
        }],
    )
    .build();

    let report = replicator.run_once().await.unwrap();
    assert_eq!(report.polled, 3, "must have polled 3 events");
    assert_eq!(report.delivered, 3, "must have delivered 3 events");

    // Target should now have 3 rows.
    assert_eq!(count(&target, "SELECT id FROM items"), 3);
}

#[tokio::test]
async fn replicator_survives_primary_restart() {
    let src = tempdir().unwrap();
    let tgt = tempdir().unwrap();

    // First batch: 3 inserts, replicate, ack committed.
    {
        let primary = Arc::new(Engine::open(src.path(), 0).unwrap());
        {
            let xid = primary.begin().unwrap();
            primary.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
            primary.commit(xid).unwrap();
        }
        primary.enable_events("t").unwrap();

        let target = Arc::new(Engine::open(tgt.path(), 0).unwrap());
        {
            let xid = target.begin().unwrap();
            target.execute_sql(xid, "CREATE TABLE t (id INT)").unwrap();
            target.commit(xid).unwrap();
        }

        for i in 1..=3 {
            let xid = primary.begin().unwrap();
            primary
                .execute_sql(xid, &format!("INSERT INTO t (id) VALUES ({i})"))
                .unwrap();
            primary.commit(xid).unwrap();
        }

        let lr = LogicalReplicator::builder(
            primary.clone(),
            target.clone(),
            "lr-restart-consumer",
            vec![TableSpec {
                table: "t".to_string(),
                key_column: "id".to_string(),
            }],
        )
        .build();

        lr.run_once().await.unwrap();
        assert_eq!(count(&target, "SELECT id FROM t"), 3);
        // primary dropped here — simulates restart
    }

    // Primary restart: reopen, insert 2 more rows.
    let primary2 = Arc::new(Engine::open(src.path(), 0).unwrap());
    primary2.enable_events("t").unwrap();
    for i in 4..=5 {
        let xid = primary2.begin().unwrap();
        primary2
            .execute_sql(xid, &format!("INSERT INTO t (id) VALUES ({i})"))
            .unwrap();
        primary2.commit(xid).unwrap();
    }

    // Reopen target too (for good measure).
    let target2 = Arc::new(Engine::open(tgt.path(), 0).unwrap());
    assert_eq!(
        count(&target2, "SELECT id FROM t"),
        3,
        "target must still have 3 rows after primary restart"
    );

    // Second run with the same consumer name — resumes from acked offset.
    let lr2 = LogicalReplicator::builder(
        primary2.clone(),
        target2.clone(),
        "lr-restart-consumer",
        vec![TableSpec {
            table: "t".to_string(),
            key_column: "id".to_string(),
        }],
    )
    .build();

    let report = lr2.run_once().await.unwrap();
    assert_eq!(
        report.polled, 2,
        "second run must poll only the 2 new events"
    );

    assert_eq!(
        count(&target2, "SELECT id FROM t"),
        5,
        "target must have all 5 rows after second run"
    );
}

#[tokio::test]
async fn tables_not_in_scope_are_skipped() {
    let src = tempdir().unwrap();
    let tgt = tempdir().unwrap();

    let primary = Arc::new(Engine::open(src.path(), 0).unwrap());
    {
        let xid = primary.begin().unwrap();
        primary.execute_sql(xid, "CREATE TABLE a (id INT)").unwrap();
        primary.execute_sql(xid, "CREATE TABLE b (id INT)").unwrap();
        primary.commit(xid).unwrap();
    }
    primary.enable_events("a").unwrap();
    primary.enable_events("b").unwrap();

    let xid = primary.begin().unwrap();
    primary
        .execute_sql(xid, "INSERT INTO a (id) VALUES (1)")
        .unwrap();
    primary
        .execute_sql(xid, "INSERT INTO b (id) VALUES (2)")
        .unwrap();
    primary.commit(xid).unwrap();

    let target = Arc::new(Engine::open(tgt.path(), 0).unwrap());
    {
        let xid = target.begin().unwrap();
        // Only create table `a` on the target — table `b` events must be skipped.
        target.execute_sql(xid, "CREATE TABLE a (id INT)").unwrap();
        target.commit(xid).unwrap();
    }

    // Replicate only table `a`.
    let lr = LogicalReplicator::builder(
        primary.clone(),
        target.clone(),
        "lr-scope-consumer",
        vec![TableSpec {
            table: "a".to_string(),
            key_column: "id".to_string(),
        }],
    )
    .build();

    let report = lr.run_once().await.unwrap();
    // 2 events were polled (one for a, one for b). Both succeed at the sink
    // level (the dispatcher counts "delivered" as "sink returned Ok"); the
    // sink itself skips the out-of-scope b event without writing to the target.
    assert_eq!(report.polled, 2);
    assert_eq!(report.dead_lettered, 0, "no events should be dead-lettered");
    // Only 1 row was actually written to the target (only table `a`).
    assert_eq!(count(&target, "SELECT id FROM a"), 1);
}
