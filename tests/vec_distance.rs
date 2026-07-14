// Item 41: `NEAR(...)` queries expose the Euclidean distance the HNSW/IVF-Flat
// scan already computes for re-ranking as a virtual `vec_distance` column, so
// callers can distinguish a genuinely close match from a weak one instead of
// every one of the k rows looking identical in quality.

use tempfile::tempdir;
use unidb::sql::executor::ExecResult as SqlResult;
use unidb::sql::logical::Literal;
use unidb::Engine;

#[test]
fn vec_distance_returned_ascending_for_known_corpus() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let xid = engine.begin().unwrap();
    engine
        .execute_sql(
            xid,
            "CREATE TABLE documents (id INT, title TEXT, embedding VECTOR(2))",
        )
        .unwrap();
    engine
        .execute_sql(xid, "CREATE INDEX idx ON documents USING HNSW (embedding)")
        .unwrap();
    engine
        .execute_sql(
            xid,
            "INSERT INTO documents (id, title, embedding) VALUES \
             (1, 'Wireless Bluetooth Headphones', [0.412, 0.0])",
        )
        .unwrap();
    engine
        .execute_sql(
            xid,
            "INSERT INTO documents (id, title, embedding) VALUES \
             (9, 'Noise Cancelling Earbuds', [0.534, 0.0])",
        )
        .unwrap();
    engine
        .execute_sql(
            xid,
            "INSERT INTO documents (id, title, embedding) VALUES \
             (5, 'Mechanical Gaming Keyboard', [1.201, 0.0])",
        )
        .unwrap();
    engine
        .execute_sql(
            xid,
            "INSERT INTO documents (id, title, embedding) VALUES \
             (2, 'Far Away Item', [50.0, 0.0])",
        )
        .unwrap();

    let results = engine
        .execute_sql(
            xid,
            "SELECT id, title, vec_distance FROM documents \
             WHERE NEAR(embedding, [0.0, 0.0], 3)",
        )
        .unwrap();

    match &results[0] {
        SqlResult::Rows { columns, rows } => {
            assert_eq!(columns, &["id", "title", "vec_distance"]);
            assert_eq!(rows.len(), 3, "k=3 must cap the result set");

            let ids: Vec<i64> = rows
                .iter()
                .map(|r| match &r[0] {
                    Literal::Int(n) => *n,
                    other => panic!("expected Int id, got {other:?}"),
                })
                .collect();
            assert_eq!(ids, vec![1, 9, 5], "expected ascending-distance order");

            let mut prev = f64::MIN;
            for row in rows {
                let dist = match &row[2] {
                    Literal::Float(d) => *d,
                    other => panic!("expected Float vec_distance, got {other:?}"),
                };
                assert!(
                    dist >= prev,
                    "distances must be non-decreasing: {dist} came after {prev}"
                );
                prev = dist;
            }

            // Exact re-ranked Euclidean distance from the stored vector — no
            // quantization error, so this matches the seeded values exactly.
            match &rows[0][2] {
                Literal::Float(d) => assert!((*d - 0.412).abs() < 1e-4, "got {d}"),
                other => panic!("expected Float, got {other:?}"),
            }
        }
        other => panic!("expected Rows, got {other:?}"),
    }
}

#[test]
fn vec_distance_outside_near_context_is_column_not_found() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let xid = engine.begin().unwrap();
    engine
        .execute_sql(xid, "CREATE TABLE documents (id INT, embedding VECTOR(2))")
        .unwrap();
    engine
        .execute_sql(
            xid,
            "INSERT INTO documents (id, embedding) VALUES (1, [0.0, 0.0])",
        )
        .unwrap();

    let err = engine
        .execute_sql(xid, "SELECT vec_distance FROM documents")
        .unwrap_err();
    assert!(
        matches!(err, unidb::error::DbError::ColumnNotFound { .. }),
        "expected COLUMN_NOT_FOUND outside a NEAR context, got {err:?}"
    );
}

#[test]
fn select_star_never_includes_vec_distance() {
    let dir = tempdir().unwrap();
    let engine = Engine::open(dir.path(), 0).unwrap();
    let xid = engine.begin().unwrap();
    engine
        .execute_sql(xid, "CREATE TABLE documents (id INT, embedding VECTOR(2))")
        .unwrap();
    engine
        .execute_sql(xid, "CREATE INDEX idx ON documents USING HNSW (embedding)")
        .unwrap();
    engine
        .execute_sql(
            xid,
            "INSERT INTO documents (id, embedding) VALUES (1, [0.0, 0.0])",
        )
        .unwrap();

    let results = engine
        .execute_sql(
            xid,
            "SELECT * FROM documents WHERE NEAR(embedding, [0.0, 0.0], 1)",
        )
        .unwrap();
    match &results[0] {
        SqlResult::Rows { columns, .. } => {
            assert!(
                !columns.iter().any(|c| c == "vec_distance"),
                "SELECT * must not surface the virtual vec_distance column: {columns:?}"
            );
        }
        other => panic!("expected Rows, got {other:?}"),
    }
}
