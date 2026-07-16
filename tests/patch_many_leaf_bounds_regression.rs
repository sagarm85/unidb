/// Regression test for the item-47 `DiskBTree::patch_many` infinite loop
/// (found while investigating a `scripts/report.sh` "indefinite hang" report,
/// item 49): a leaf's *current* entries don't have to span its full
/// structural key range (e.g. right after a split), so the very first patch
/// in a leaf-group can legitimately have a key outside
/// `entries.first()/last()`. The old code gated its find-or-fallback step on
/// that bounds check even for the first entry, so the loop index never
/// advanced and the whole call spun forever, single-threaded, burning CPU
/// with no output — exactly what `scripts/report.sh` looked like from the
/// outside.
///
/// Reproduces the exact shape that hung in practice: a table indexed on `k`,
/// enough rows to force B-tree leaf splits, an unchanged-key `UPDATE` over
/// roughly half the table (the same `UPDATE t SET body = ... WHERE k < N/2`
/// `benches/decompose.rs` Table 3 runs). Runs on a background thread with a
/// deadline so a real regression fails the test cleanly instead of hanging
/// the whole suite.
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tempfile::tempdir;
use unidb::sql::logical::Literal;
use unidb::Engine;

const ROWS: u64 = 10_000;
const DEADLINE_SECS: u64 = 30;

fn build_table(e: &Engine, rows: u64) {
    e.set_deferred_sync(true);
    let x = e.begin().unwrap();
    e.execute_sql(x, "CREATE TABLE t (id INT, k INT, g INT, body TEXT)")
        .unwrap();
    e.execute_sql(x, "CREATE INDEX t_k ON t USING BTREE (k)")
        .unwrap();
    e.commit(x).unwrap();
    let prep = e
        .prepare("INSERT INTO t (id, k, g, body) VALUES ($1, $2, $3, $4)")
        .unwrap();
    const BATCH: u64 = 500;
    let mut i = 0u64;
    while i < rows {
        let x = e.begin().unwrap();
        for j in i..(i + BATCH).min(rows) {
            e.execute_prepared(
                x,
                &prep,
                &[
                    Literal::Int(j as i64),
                    Literal::Int(j as i64),
                    Literal::Int((j % 4) as i64),
                    Literal::Text(format!("body_{j}")),
                ],
            )
            .unwrap();
        }
        e.commit(x).unwrap();
        i += BATCH;
    }
    let x = e.begin().unwrap();
    e.execute_sql(x, "ANALYZE t").unwrap();
    e.commit(x).unwrap();
}

#[test]
fn unchanged_key_update_does_not_hang_across_leaf_splits() {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let dir = tempdir().unwrap();
        let e = Engine::open(dir.path(), 0).unwrap();
        build_table(&e, ROWS);

        let half = (ROWS / 2) as i64;
        let x = e.begin().unwrap();
        let start = Instant::now();
        let res = e
            .execute_sql(
                x,
                &format!("UPDATE t SET body = 'updated' WHERE k < {half}"),
            )
            .unwrap();
        e.commit(x).unwrap();
        let elapsed = start.elapsed();

        let affected: usize = res
            .iter()
            .map(|r| match r {
                unidb::SqlResult::Updated { count } => *count,
                other => panic!("expected an Updated result, got {other:?}"),
            })
            .sum();
        let _ = tx.send((affected, elapsed));
    });

    match rx.recv_timeout(Duration::from_secs(DEADLINE_SECS)) {
        Ok((affected, elapsed)) => {
            assert_eq!(
                affected as u64,
                ROWS / 2,
                "UPDATE should have touched exactly half the table"
            );
            assert!(
                elapsed < Duration::from_secs(DEADLINE_SECS),
                "UPDATE completed but took {elapsed:?} — investigate before raising the deadline"
            );
        }
        Err(_) => panic!(
            "HANG: unchanged-key UPDATE across {ROWS} rows did not complete within \
             {DEADLINE_SECS}s — DiskBTree::patch_many infinite-loop regression (item 47/49)"
        ),
    }
}
