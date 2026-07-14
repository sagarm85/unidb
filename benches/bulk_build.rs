// Item 40 benchmark: CREATE INDEX backfill wall-time (sort-then-bulk-load).
//
// Measures the wall-clock cost of `CREATE INDEX ... USING BTREE (customer_id)`
// on a pre-populated 540k-row table — the acceptance criterion from item 40.
// Run before and after the fix to capture the before/after numbers.
//
// Run with:
//   UNIDB_BUFFER_POOL_PAGES=1000000 cargo bench --bench bulk_build --release
//   (or: cargo bench --bench bulk_build for a quick non-release run)

use std::time::Instant;

use tempfile::tempdir;
use unidb::Engine;

const ROWS: u64 = 540_000;
const BATCH: u64 = 1_000;

fn seed_table(engine: &Engine, rows: u64) {
    let xid = engine.begin().unwrap();
    engine
        .execute_sql(xid, "CREATE TABLE orders (customer_id INT, amount INT)")
        .unwrap();
    engine.commit(xid).unwrap();

    let mut i = 0u64;
    while i < rows {
        let end = (i + BATCH).min(rows);
        // Shuffle the customer_id values so the baseline sees random-key
        // inserts (representative of real-world heap order, worst case for
        // unsorted B-tree inserts). Use a simple LCG pattern: not perfectly
        // random, but non-monotone and cheap.
        let values: Vec<String> = (i..end)
            .map(|j| {
                // LCG: a=1664525, c=1013904223, mod=2^32 — non-monotone ids
                let cid = (j.wrapping_mul(1_664_525).wrapping_add(1_013_904_223)) % rows;
                format!("({cid}, {j})")
            })
            .collect();
        let xid = engine.begin().unwrap();
        engine
            .execute_sql(
                xid,
                &format!(
                    "INSERT INTO orders (customer_id, amount) VALUES {}",
                    values.join(", ")
                ),
            )
            .unwrap();
        engine.commit(xid).unwrap();
        i = end;
    }
}

fn main() {
    let pool_pages: usize = std::env::var("UNIDB_BUFFER_POOL_PAGES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000_000);

    println!("Item 40 — CREATE INDEX backfill benchmark");
    println!("  rows        : {}", ROWS);
    println!("  pool_pages  : {}", pool_pages);
    println!();

    let dir = tempdir().unwrap();
    let engine = Engine::open_with_pool_capacity(dir.path(), 0, pool_pages).unwrap();

    print!("Seeding {} rows… ", ROWS);
    let t0 = Instant::now();
    seed_table(&engine, ROWS);
    let seed_secs = t0.elapsed().as_secs_f64();
    println!("done in {:.1}s", seed_secs);

    print!("CREATE INDEX ON orders USING BTREE (customer_id)… ");
    std::io::Write::flush(&mut std::io::stdout()).ok();
    let t1 = Instant::now();
    let xid = engine.begin().unwrap();
    engine
        .execute_sql(
            xid,
            "CREATE INDEX idx_cid ON orders USING BTREE (customer_id)",
        )
        .unwrap();
    engine.commit(xid).unwrap();
    let index_secs = t1.elapsed().as_secs_f64();
    println!("done in {:.3}s", index_secs);

    println!();
    println!("| Rows    | Buffer pool pages | CREATE INDEX wall-time |");
    println!("|---------|-------------------|------------------------|");
    println!(
        "| {:>7} | {:>17} | {:>20.3}s |",
        ROWS, pool_pages, index_secs
    );
}
