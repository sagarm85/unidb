//! Pure SQL-string builders for the two commands. Kept separate from I/O so
//! they can be unit-tested without a server or an embedding endpoint.
//!
//! The generated SQL targets UniDB's subset: a `VECTOR(n)` literal is written
//! as a bracketed float list (`[0.1, 0.2]`), and nearest-neighbor search uses
//! the `NEAR(column, [...], k)` operator (M2).

/// Render a vector as a UniDB `[f1, f2, ...]` literal.
pub fn vector_literal(vector: &[f32]) -> String {
    let mut out = String::from("[");
    for (i, v) in vector.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        // `{}` on f32 never emits `inf`/`nan` for finite embedding values and
        // keeps enough precision for ranking; embeddings are already f32.
        out.push_str(&v.to_string());
    }
    out.push(']');
    out
}

/// Escape a string for a single-quoted SQL literal (double any `'`).
pub fn sql_escape(text: &str) -> String {
    text.replace('\'', "''")
}

/// `INSERT INTO <table> (<id_col>, <text_col>, <vec_col>) VALUES (<id>, '<text>', [<vec>])`.
pub fn insert_sql(
    table: &str,
    id_col: &str,
    text_col: &str,
    vec_col: &str,
    id: i64,
    text: &str,
    vector: &[f32],
) -> String {
    format!(
        "INSERT INTO {table} ({id_col}, {text_col}, {vec_col}) VALUES ({id}, '{}', {})",
        sql_escape(text),
        vector_literal(vector),
    )
}

/// `SELECT <id_col>, <text_col> FROM <table> WHERE NEAR(<vec_col>, [<vec>], <k>)`.
pub fn search_sql(
    table: &str,
    id_col: &str,
    text_col: &str,
    vec_col: &str,
    vector: &[f32],
    k: usize,
) -> String {
    format!(
        "SELECT {id_col}, {text_col} FROM {table} WHERE NEAR({vec_col}, {}, {k})",
        vector_literal(vector),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_literal_formats_brackets_and_commas() {
        assert_eq!(vector_literal(&[]), "[]");
        assert_eq!(vector_literal(&[1.0]), "[1]");
        assert_eq!(vector_literal(&[0.5, -1.5]), "[0.5, -1.5]");
    }

    #[test]
    fn sql_escape_doubles_single_quotes() {
        assert_eq!(sql_escape("it's"), "it''s");
        assert_eq!(sql_escape("plain"), "plain");
    }

    #[test]
    fn insert_sql_shape() {
        let sql = insert_sql("docs", "id", "content", "embedding", 7, "hi", &[0.1, 0.2]);
        assert_eq!(
            sql,
            "INSERT INTO docs (id, content, embedding) VALUES (7, 'hi', [0.1, 0.2])"
        );
    }

    #[test]
    fn insert_sql_escapes_text() {
        let sql = insert_sql("docs", "id", "content", "embedding", 1, "a'b", &[1.0]);
        assert!(sql.contains("'a''b'"), "got: {sql}");
    }

    #[test]
    fn search_sql_shape() {
        let sql = search_sql("docs", "id", "content", "embedding", &[0.1, 0.2], 5);
        assert_eq!(
            sql,
            "SELECT id, content FROM docs WHERE NEAR(embedding, [0.1, 0.2], 5)"
        );
    }
}
