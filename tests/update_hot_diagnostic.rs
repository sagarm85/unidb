/// UPDATE HOT diagnostic — mirrors the Docker Table 3 UPDATE HOT scenario exactly.
///
/// Table 3 schema: `(id INT, k INT, g INT, body TEXT)` with secondary BTREE on k.
/// No PRIMARY KEY, no UNIQUE → hot_eligible = true → hot_update_many is used.
///
/// Sequence: INSERT N rows → ANALYZE → UPDATE SET body WHERE k < N/2 (50%).
/// A3 gate: 50% selectivity → full-scan path (index path blocked by selectivity gate).
///
/// This test measures:
///   (A) UPDATE HOT (hot_update_many, batch Phase A + B)
///   (B) UPDATE non-HOT (SET k, forces B-tree index maintenance)
use std::time::Instant;
use tempfile::tempdir;
use unidb::sql::executor::ExecResult;
use unidb::sql::logical::Literal;
use unidb::Engine;

const ROWS: u64 = 100_000; // Match Docker bench: 100k base rows + 100k INSERT bench = 200k total

fn open_engine(dir: &std::path::Path) -> Engine {
    Engine::open(dir, 0).unwrap()
}

fn build_table(e: &Engine, rows: u64) {
    e.set_deferred_sync(true);
    let x = e.begin().unwrap();
    // Mirror Docker bench schema exactly — no PRIMARY KEY, no UNIQUE.
    e.execute_sql(x, "CREATE TABLE t (id INT, k INT, g INT, body TEXT)")
        .unwrap();
    e.execute_sql(x, "CREATE INDEX t_k ON t USING BTREE (k)")
        .unwrap();
    e.commit(x).unwrap();

    const BATCH: u64 = 500;
    let prep = e
        .prepare("INSERT INTO t (id, k, g, body) VALUES ($1, $2, $3, $4)")
        .unwrap();
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
                    Literal::Int((j as i64) % 100),
                    Literal::Text(format!("b{j}")),
                ],
            )
            .unwrap();
        }
        e.commit(x).unwrap();
        i += BATCH;
    }
    // ANALYZE after full INSERT — fresh stats, 50% selectivity for UPDATE WHERE k < N/2
    // means A3 does NOT fire → full-scan path → hot_update_many.
    let x = e.begin().unwrap();
    e.execute_sql(x, "ANALYZE t").unwrap();
    e.commit(x).unwrap();
}

fn run_update_hot(e: &Engine) -> (usize, std::time::Duration, u64) {
    let half = (ROWS / 2) as i64;
    let wal_before = e.wal_total_bytes_appended();
    let t0 = Instant::now();
    let x = e.begin().unwrap();
    let res = e
        .execute_sql(
            x,
            &format!("UPDATE t SET body = 'updated' WHERE k < {half}"),
        )
        .unwrap();
    e.commit(x).unwrap();
    let elapsed = t0.elapsed();
    let wal_delta = e.wal_total_bytes_appended() - wal_before;
    let count = res
        .iter()
        .find_map(|r| {
            if let ExecResult::Updated { count } = r {
                Some(*count)
            } else {
                None
            }
        })
        .unwrap_or(0);
    (count, elapsed, wal_delta)
}

fn run_update_nonhot(e: &Engine) -> (usize, std::time::Duration, u64) {
    let lo = ROWS as i64;
    let hi = lo + (ROWS / 2) as i64;
    let wal_before = e.wal_total_bytes_appended();
    let t0 = Instant::now();
    let x = e.begin().unwrap();
    // SET k (indexed column) → B-tree must be updated → not hot_eligible.
    let res = e
        .execute_sql(
            x,
            &format!("UPDATE t SET k = k + 1 WHERE k >= {lo} AND k < {hi}"),
        )
        .unwrap();
    e.commit(x).unwrap();
    let elapsed = t0.elapsed();
    let wal_delta = e.wal_total_bytes_appended() - wal_before;
    let count = res
        .iter()
        .find_map(|r| {
            if let ExecResult::Updated { count } = r {
                Some(*count)
            } else {
                None
            }
        })
        .unwrap_or(0);
    (count, elapsed, wal_delta)
}

/// Scenario A: UPDATE HOT — mirrors Docker Table 3 "UPDATE HOT-eligible".
/// Exercises hot_update_many (batch Phase A + B, one mini-txn per page group).
#[test]
fn update_hot_scenario_a_hot_eligible() {
    let dir = tempdir().unwrap();
    let e = open_engine(dir.path());
    build_table(&e, ROWS);

    let (count, elapsed, wal_delta) = run_update_hot(&e);
    let rec_s = count as f64 / elapsed.as_secs_f64();
    let wal_b_per_row = if count > 0 {
        wal_delta / count as u64
    } else {
        0
    };
    println!(
        "[UPDATE HOT A] hot_eligible (Table 3 mirror): {} rows in {:.3}s → {:.0} rec/s | WAL {wal_b_per_row} B/row",
        count, elapsed.as_secs_f64(), rec_s,
    );
    assert_eq!(
        count,
        (ROWS / 2) as usize,
        "should update exactly half the rows"
    );
}

/// Scenario B: UPDATE non-HOT — SET k (indexed column), B-tree maintained.
/// Insert extra rows first so we have a target range [N, 3N/2).
#[test]
fn update_hot_scenario_b_nonhot() {
    let dir = tempdir().unwrap();
    let e = open_engine(dir.path());
    build_table(&e, ROWS);

    // Insert extra rows k in [ROWS, 3N/2) — these will be the non-HOT UPDATE targets.
    let extra = ROWS / 2;
    let prep = e
        .prepare("INSERT INTO t (id, k, g, body) VALUES ($1, $2, $3, $4)")
        .unwrap();
    const BATCH: u64 = 500;
    let mut i = ROWS;
    while i < ROWS + extra {
        let x = e.begin().unwrap();
        for j in i..(i + BATCH).min(ROWS + extra) {
            e.execute_prepared(
                x,
                &prep,
                &[
                    Literal::Int(j as i64),
                    Literal::Int(j as i64),
                    Literal::Int((j as i64) % 100),
                    Literal::Text(format!("b{j}")),
                ],
            )
            .unwrap();
        }
        e.commit(x).unwrap();
        i += BATCH;
    }

    let (count, elapsed, wal_delta) = run_update_nonhot(&e);
    let rec_s = count as f64 / elapsed.as_secs_f64();
    let wal_b_per_row = if count > 0 {
        wal_delta / count as u64
    } else {
        0
    };
    println!(
        "[UPDATE non-HOT B]: {} rows in {:.3}s → {:.0} rec/s | WAL {wal_b_per_row} B/row",
        count,
        elapsed.as_secs_f64(),
        rec_s,
    );
    assert_eq!(
        count, extra as usize,
        "should update exactly the extra rows"
    );
}
