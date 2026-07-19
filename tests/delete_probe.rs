use std::sync::Arc;
use std::time::Instant;
use tempfile::tempdir;
use unidb::sql::logical::Literal;
use unidb::{AutoCheckpointConfig, Engine};

fn bench_engine(dir: &std::path::Path) -> Arc<Engine> {
    let e = Arc::new(Engine::open_with_pool_capacity(dir, 0, 2_000_000).unwrap());
    // 512 MiB threshold: prevents mid-run checkpoints that clear fpi_logged
    // and force FPI re-logging on the DELETE phase (matches bench_engine_open).
    e.set_auto_checkpoint_config(AutoCheckpointConfig {
        max_wal_size: 512 * 1024 * 1024,
        ..Default::default()
    });
    e
}

#[test]
fn delete_selected_probe() {
    let n: u64 = 100_000;
    let dir = tempdir().unwrap();
    let se = bench_engine(dir.path());
    se.set_deferred_sync(true);

    let x = se.begin().unwrap();
    se.execute_sql(x, "CREATE TABLE t (id INT, k INT, g INT, body TEXT)")
        .unwrap();
    se.execute_sql(x, "CREATE INDEX t_k ON t USING BTREE (k)")
        .unwrap();
    se.commit(x).unwrap();

    let ins = se
        .prepare("INSERT INTO t (id, k, g, body) VALUES ($1, $2, $3, $4)")
        .unwrap();
    let mut x = se.begin().unwrap();
    for i in 0..n {
        se.execute_prepared(
            x,
            &ins,
            &[
                Literal::Int(i as i64),
                Literal::Int(i as i64),
                Literal::Int((i as i64) % 8),
                Literal::Text(format!("b{i}")),
            ],
        )
        .unwrap();
        if (i + 1) % 5_000 == 0 {
            se.commit(x).unwrap();
            x = se.begin().unwrap();
        }
    }
    se.commit(x).unwrap();

    let wal_after_build = se.wal_total_bytes_appended();
    println!(
        "[probe] after sql_build_crud: WAL {:.2} MB",
        wal_after_build as f64 / 1e6
    );

    let mut x = se.begin().unwrap();
    for i in 0..n {
        let k = n as i64 + i as i64;
        se.execute_prepared(
            x,
            &ins,
            &[
                Literal::Int(k),
                Literal::Int(k),
                Literal::Int(k % 8),
                Literal::Text(format!("b{k}")),
            ],
        )
        .unwrap();
        if (i + 1) % 5_000 == 0 {
            se.commit(x).unwrap();
            x = se.begin().unwrap();
        }
    }
    se.commit(x).unwrap();

    let wal_after_insert = se.wal_total_bytes_appended();
    println!(
        "[probe] after sql_crud_insert: WAL {:.2} MB delta",
        (wal_after_insert - wal_after_build) as f64 / 1e6
    );

    let ax = se.begin().unwrap();
    se.execute_sql(ax, "ANALYZE t").unwrap();
    se.commit(ax).unwrap();

    let half = (n / 2) as i64;
    let wal_pre_uph = se.wal_total_bytes_appended();
    let t0 = Instant::now();
    let x = se.begin().unwrap();
    se.execute_sql(x, &format!("UPDATE t SET body = 'upd' WHERE k < {half}"))
        .unwrap();
    se.commit(x).unwrap();
    let wal_post_uph = se.wal_total_bytes_appended();
    println!(
        "[probe] UPDATE HOT ({}k rows): {:.3}s | WAL delta {:.2} MB ({:.0} B/row)",
        n / 2 / 1000,
        t0.elapsed().as_secs_f64(),
        (wal_post_uph - wal_pre_uph) as f64 / 1e6,
        (wal_post_uph - wal_pre_uph) as f64 / (n / 2) as f64
    );

    // UPDATE non-HOT: SET k = k+1 (indexed col → B-tree maintenance required)
    let wal_pre_upn = se.wal_total_bytes_appended();
    let t0 = Instant::now();
    let x = se.begin().unwrap();
    se.execute_sql(
        x,
        &format!(
            "UPDATE t SET k = k + 1 WHERE k >= {n} AND k < {}",
            n as i64 + half
        ),
    )
    .unwrap();
    let t_sql = t0.elapsed();
    se.commit(x).unwrap();
    let t_total = t0.elapsed();
    let wal_post_upn = se.wal_total_bytes_appended();
    println!("[probe] UPDATE non-HOT ({}k rows): {:.3}s (sql={:.3}s commit={:.3}s) | WAL {:.2} MB ({:.0} B/row)",
             n/2/1000, t_total.as_secs_f64(), t_sql.as_secs_f64(),
             (t_total - t_sql).as_secs_f64(),
             (wal_post_upn - wal_pre_upn) as f64 / 1e6,
             (wal_post_upn - wal_pre_upn) as f64 / (n/2) as f64);

    // UPDATE heap-only (SET g = g+1, g NOT indexed): isolates heap-write cost without B-tree
    // Runs on the second batch (k=100001..150000 after the non-HOT update above).
    let wal_pre_uph2 = se.wal_total_bytes_appended();
    let t0 = Instant::now();
    let x = se.begin().unwrap();
    // Use g (not indexed) on the same row range — measures heap-only update cost
    se.execute_sql(
        x,
        &format!(
            "UPDATE t SET g = g + 1 WHERE k >= {} AND k < {}",
            n as i64 + 1,
            n as i64 + half + 1
        ),
    )
    .unwrap();
    let t_sql2 = t0.elapsed();
    se.commit(x).unwrap();
    let t_total2 = t0.elapsed();
    let wal_post_uph2 = se.wal_total_bytes_appended();
    println!("[probe] UPDATE heap-only ({}k rows, no B-tree): {:.3}s (sql={:.3}s commit={:.3}s) | WAL {:.2} MB ({:.0} B/row)",
             n/2/1000, t_total2.as_secs_f64(), t_sql2.as_secs_f64(),
             (t_total2 - t_sql2).as_secs_f64(),
             (wal_post_uph2 - wal_pre_uph2) as f64 / 1e6,
             (wal_post_uph2 - wal_pre_uph2) as f64 / (n/2) as f64);

    let wal_before = se.wal_total_bytes_appended();
    let t0 = Instant::now();
    let x = se.begin().unwrap();
    se.execute_sql(x, &format!("DELETE FROM t WHERE k >= {n}"))
        .unwrap();
    se.commit(x).unwrap();
    let elapsed = t0.elapsed();
    let wal_after = se.wal_total_bytes_appended();
    let wal_bytes = wal_after - wal_before;

    let rec_s = n as f64 / elapsed.as_secs_f64();
    let wal_per_row = wal_bytes / n;
    println!(
        "[probe] DELETE selected ({}k rows): {:.3}s → {:.0} rec/s | WAL {:.2} MB ({} B/row)",
        n / 1000,
        elapsed.as_secs_f64(),
        rec_s,
        wal_bytes as f64 / 1e6,
        wal_per_row
    );
}
