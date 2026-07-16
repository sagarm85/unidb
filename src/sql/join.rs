//! Phase-4 join algorithms over materialized [`Batch`]es: hash join (with
//! Grace spill-to-disk past a row budget), sort-merge join, and block
//! nested-loop join. Index-nested-loop lives in the executor (it needs storage
//! access to probe the on-disk B-Tree per outer row) — see
//! [`crate::sql::executor`].
//!
//! All three emit rows in `left_columns ++ right_columns` order and honour the
//! join type (Inner/Left/Right/Cross). A non-equi ON residual is applied as
//! part of the match test, so `LEFT JOIN ... ON a.x = b.y AND a.z < b.w`
//! null-extends a left row that has no right row satisfying *both*.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;

use crate::error::{DbError, Result};
use crate::sql::logical::Literal;
use crate::sql::plan::{eval_predicate, eval_qexpr, join_key_bytes, key_ord, Batch, ColumnRef};
use crate::sql::query::{JoinType, QExpr};

/// Callback the Grace path uses to emit a combined row: `(probe_row,
/// Some(build_row) | None)`, returning whether the row was actually emitted
/// (the residual can veto it).
type EmitFn<'a> = dyn FnMut(&[Literal], Option<&[Literal]>) -> Result<bool> + 'a;

/// The combined output schema of a join is the left schema followed by the
/// right schema.
fn combined_schema(left: &[ColumnRef], right: &[ColumnRef]) -> Vec<ColumnRef> {
    let mut s = left.to_vec();
    s.extend_from_slice(right);
    s
}

fn nulls(n: usize) -> Vec<Literal> {
    vec![Literal::Null; n]
}

fn key_of(keys: &[QExpr], schema: &[ColumnRef], row: &[Literal]) -> Result<Vec<Literal>> {
    keys.iter().map(|k| eval_qexpr(k, schema, row)).collect()
}

/// Whether a matched `left ++ right` combined row passes the non-equi residual
/// (always true when there is none).
fn residual_ok(
    residual: &Option<QExpr>,
    out_schema: &[ColumnRef],
    combined: &[Literal],
) -> Result<bool> {
    match residual {
        None => Ok(true),
        Some(r) => eval_predicate(r, out_schema, combined),
    }
}

// ── hash join ────────────────────────────────────────────────────────────────

/// Equi-join `left` and `right` on `left_keys[i] = right_keys[i]`, applying
/// `residual` (the non-equi part of ON) to each candidate pair. Builds an
/// in-memory hash table when the build side fits `mem_rows`, otherwise falls
/// back to Grace partitioning (both sides spilled to temp files, joined
/// partition-by-partition) so peak memory stays bounded.
#[allow(clippy::too_many_arguments)]
pub fn hash_join(
    left: Batch,
    right: Batch,
    join_type: JoinType,
    left_keys: &[QExpr],
    right_keys: &[QExpr],
    residual: &Option<QExpr>,
    mem_rows: usize,
) -> Result<Batch> {
    let out_schema = combined_schema(&left.schema, &right.schema);

    // Orientation: build the hash table on the side we don't need to emit
    // unmatched rows from. Inner/Left probe the left (so build = right); Right
    // probes the right (build = left).
    let build_is_right = !matches!(join_type, JoinType::Right);
    let emit_unmatched_probe = matches!(join_type, JoinType::Left | JoinType::Right);

    let (build, build_keys, probe, probe_keys) = if build_is_right {
        (&right, right_keys, &left, left_keys)
    } else {
        (&left, left_keys, &right, right_keys)
    };

    let left_len = left.schema.len();
    let right_len = right.schema.len();

    let mut out_rows = Vec::new();
    let mut emit = |probe_row: &[Literal], build_row: Option<&[Literal]>| -> Result<bool> {
        // Assemble left ++ right in canonical order regardless of orientation.
        let combined: Vec<Literal> = if build_is_right {
            let mut c = probe_row.to_vec();
            match build_row {
                Some(b) => c.extend_from_slice(b),
                None => c.extend(nulls(right_len)),
            }
            c
        } else {
            let mut c = match build_row {
                Some(b) => b.to_vec(),
                None => nulls(left_len),
            };
            c.extend_from_slice(probe_row);
            c
        };
        if build_row.is_some() && !residual_ok(residual, &out_schema, &combined)? {
            return Ok(false);
        }
        out_rows.push(combined);
        Ok(true)
    };

    if build.rows.len() <= mem_rows {
        // Fast path: single integer key, inner join, no outer-unmatched emission.
        // Uses HashMap<i64, Vec<usize>> (build-row indices) to avoid Vec<u8> key
        // encoding and up-front row cloning into the hash table.
        let use_i64_fast = !emit_unmatched_probe
            && build_keys.len() == 1
            && probe_keys.len() == 1
            && !build.rows.is_empty()
            && matches!(
                eval_qexpr(&build_keys[0], &build.schema, &build.rows[0])?,
                Literal::Int(_)
            );

        if use_i64_fast {
            let mut table: HashMap<i64, Vec<usize>> =
                HashMap::with_capacity(build.rows.len());
            for (idx, row) in build.rows.iter().enumerate() {
                if let Literal::Int(k) = eval_qexpr(&build_keys[0], &build.schema, row)? {
                    table.entry(k).or_default().push(idx);
                }
                // NULL key: skip (SQL equi-join semantics)
            }
            for prow in &probe.rows {
                if let Literal::Int(k) = eval_qexpr(&probe_keys[0], &probe.schema, prow)? {
                    if let Some(indices) = table.get(&k) {
                        for &idx in indices {
                            emit(prow, Some(&build.rows[idx]))?;
                        }
                    }
                }
                // NULL probe key: no match
            }
        } else {
            // General in-memory path: encode keys as bytes, clone build rows.
            let mut table: HashMap<Vec<u8>, Vec<Vec<Literal>>> = HashMap::new();
            for row in &build.rows {
                let key = key_of(build_keys, &build.schema, row)?;
                if let Some(bytes) = join_key_bytes(&key) {
                    table.entry(bytes).or_default().push(row.clone());
                }
            }
            for prow in &probe.rows {
                let key = key_of(probe_keys, &probe.schema, prow)?;
                let mut matched = false;
                if let Some(bytes) = join_key_bytes(&key) {
                    if let Some(bucket) = table.get(&bytes) {
                        for brow in bucket {
                            if emit(prow, Some(brow))? {
                                matched = true;
                            }
                        }
                    }
                }
                if !matched && emit_unmatched_probe {
                    emit(prow, None)?;
                }
            }
        }
    } else {
        grace_hash_join(
            build,
            build_keys,
            probe,
            probe_keys,
            emit_unmatched_probe,
            mem_rows,
            &mut emit,
        )?;
    }

    Ok(Batch {
        schema: out_schema,
        rows: out_rows,
    })
}

/// Grace hash join: partition both inputs by `hash(key) % P` into temp files,
/// then join each partition independently with an in-memory table. Because a
/// probe row and all its potential build matches hash to the same partition,
/// per-partition matching is complete — including unmatched-probe detection for
/// outer joins.
fn grace_hash_join(
    build: &Batch,
    build_keys: &[QExpr],
    probe: &Batch,
    probe_keys: &[QExpr],
    emit_unmatched_probe: bool,
    mem_rows: usize,
    emit: &mut EmitFn,
) -> Result<()> {
    // Enough partitions that each build partition is expected to fit in the
    // memory budget (×2 headroom for hash skew), clamped to a sane range.
    let target = build.rows.len().div_ceil(mem_rows.max(1)) * 2;
    let partitions = target.clamp(4, 256);
    let spill = SpillDir::new()?;

    let mut build_w = spill.writers("b", partitions)?;
    for row in &build.rows {
        let key = key_of(build_keys, &build.schema, row)?;
        // NULL-key build rows never match — drop them entirely.
        if let Some(bytes) = join_key_bytes(&key) {
            let p = hash_bytes(&bytes) as usize % partitions;
            write_row(&mut build_w[p], row)?;
        }
    }
    let mut probe_w = spill.writers("p", partitions)?;
    for row in &probe.rows {
        let key = key_of(probe_keys, &probe.schema, row)?;
        match join_key_bytes(&key) {
            Some(bytes) => {
                let p = hash_bytes(&bytes) as usize % partitions;
                write_row(&mut probe_w[p], row)?;
            }
            // NULL-key probe row: never matches; emit now if outer.
            None => {
                if emit_unmatched_probe {
                    emit(row, None)?;
                }
            }
        }
    }
    for w in build_w.iter_mut().chain(probe_w.iter_mut()) {
        w.flush()?;
    }
    drop(build_w);
    drop(probe_w);

    for p in 0..partitions {
        let build_rows = read_rows(&spill.path("b", p))?;
        let mut table: HashMap<Vec<u8>, Vec<Vec<Literal>>> = HashMap::new();
        for row in &build_rows {
            let key = key_of(build_keys, &build.schema, row)?;
            if let Some(bytes) = join_key_bytes(&key) {
                table.entry(bytes).or_default().push(row.clone());
            }
        }
        for prow in read_rows(&spill.path("p", p))? {
            let key = key_of(probe_keys, &probe.schema, &prow)?;
            let mut matched = false;
            if let Some(bytes) = join_key_bytes(&key) {
                if let Some(bucket) = table.get(&bytes) {
                    for brow in bucket {
                        if emit(&prow, Some(brow))? {
                            matched = true;
                        }
                    }
                }
            }
            if !matched && emit_unmatched_probe {
                emit(&prow, None)?;
            }
        }
    }
    Ok(())
}

fn hash_bytes(b: &[u8]) -> u64 {
    // FNV-1a — small, dependency-free, good enough to partition.
    let mut h: u64 = 0xcbf29ce484222325;
    for &byte in b {
        h ^= byte as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// A temp directory for join spill files, removed on drop.
struct SpillDir {
    dir: PathBuf,
}

impl SpillDir {
    fn new() -> Result<Self> {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("unidb-hashjoin-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn path(&self, side: &str, p: usize) -> PathBuf {
        self.dir.join(format!("{side}-{p}"))
    }

    fn writers(&self, side: &str, partitions: usize) -> Result<Vec<BufWriter<File>>> {
        (0..partitions)
            .map(|p| Ok(BufWriter::new(File::create(self.path(side, p))?)))
            .collect()
    }
}

impl Drop for SpillDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn write_row(w: &mut BufWriter<File>, row: &[Literal]) -> Result<()> {
    let line = serde_json::to_string(row)
        .map_err(|e| DbError::SqlPlan(format!("join spill encode: {e}")))?;
    w.write_all(line.as_bytes())?;
    w.write_all(b"\n")?;
    Ok(())
}

fn read_rows(path: &std::path::Path) -> Result<Vec<Vec<Literal>>> {
    let file = match File::open(path) {
        Ok(f) => f,
        // An empty partition may never have been written to.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let row: Vec<Literal> = serde_json::from_str(&line)
            .map_err(|e| DbError::SqlPlan(format!("join spill decode: {e}")))?;
        out.push(row);
    }
    Ok(out)
}

// ── sort-merge join ──────────────────────────────────────────────────────────

/// Sort-merge equi-join: sort both inputs on the join keys, then merge. Correct
/// for any input order (it sorts first); the P4.d optimizer selects it when the
/// inputs are already ordered (e.g. from index scans) so the sort is cheap.
/// Duplicate keys on both sides are handled by emitting the full cross-product
/// of each equal-key run.
#[allow(clippy::too_many_arguments)]
pub fn merge_join(
    left: Batch,
    right: Batch,
    join_type: JoinType,
    left_keys: &[QExpr],
    right_keys: &[QExpr],
    residual: &Option<QExpr>,
) -> Result<Batch> {
    let out_schema = combined_schema(&left.schema, &right.schema);
    let right_len = right.schema.len();
    let left_len = left.schema.len();

    // Materialize (key, row) pairs, dropping NULL-key rows from the matchable
    // set (they can only ever appear as unmatched outer rows).
    let mut lk = keyed(&left, left_keys)?;
    let mut rk = keyed(&right, right_keys)?;
    sort_keyed(&mut lk);
    sort_keyed(&mut rk);

    let mut out_rows = Vec::new();
    let emit_unmatched_left = matches!(join_type, JoinType::Left);
    let emit_unmatched_right = matches!(join_type, JoinType::Right);

    let mut i = 0;
    let mut j = 0;
    while i < lk.len() && j < rk.len() {
        match key_ord(&lk[i].0, &rk[j].0) {
            Some(std::cmp::Ordering::Less) => {
                if emit_unmatched_left {
                    out_rows.push(extend(&lk[i].1, nulls(right_len)));
                }
                i += 1;
            }
            Some(std::cmp::Ordering::Greater) => {
                if emit_unmatched_right {
                    out_rows.push(prepend(nulls(left_len), &rk[j].1));
                }
                j += 1;
            }
            Some(std::cmp::Ordering::Equal) => {
                // Gather the equal-key runs on both sides and cross-product them.
                let i_end = run_end(&lk, i);
                let j_end = run_end(&rk, j);
                let l_run = &lk[i..i_end];
                let r_run = &rk[j..j_end];
                for (_, lrow) in l_run {
                    let mut any = false;
                    for (_, rrow) in r_run {
                        let combined = extend(lrow, rrow.clone());
                        if residual_ok(residual, &out_schema, &combined)? {
                            out_rows.push(combined);
                            any = true;
                        }
                    }
                    if !any && emit_unmatched_left {
                        out_rows.push(extend(lrow, nulls(right_len)));
                    }
                }
                if emit_unmatched_right {
                    // A right row in this run is unmatched only if no left row
                    // paired with it under the residual. With no residual every
                    // right-run row matched; with a residual, re-check.
                    for (_, rrow) in r_run {
                        let matched = residual.is_none()
                            || l_run.iter().any(|(_, lrow)| {
                                let combined = extend(lrow, rrow.clone());
                                residual_ok(residual, &out_schema, &combined).unwrap_or(false)
                            });
                        if !matched {
                            out_rows.push(prepend(nulls(left_len), rrow));
                        }
                    }
                }
                i = i_end;
                j = j_end;
            }
            // Unorderable keys shouldn't occur (NULL keys were filtered); treat
            // as non-matching to stay total.
            None => {
                i += 1;
                j += 1;
            }
        }
    }
    if emit_unmatched_left {
        while i < lk.len() {
            out_rows.push(extend(&lk[i].1, nulls(right_len)));
            i += 1;
        }
    }
    if emit_unmatched_right {
        while j < rk.len() {
            out_rows.push(prepend(nulls(left_len), &rk[j].1));
            j += 1;
        }
    }

    Ok(Batch {
        schema: out_schema,
        rows: out_rows,
    })
}

type Keyed = Vec<(Vec<Literal>, Vec<Literal>)>;

fn keyed(batch: &Batch, keys: &[QExpr]) -> Result<Keyed> {
    let mut out = Vec::with_capacity(batch.rows.len());
    for row in &batch.rows {
        let k = key_of(keys, &batch.schema, row)?;
        // Drop NULL-key rows from the matchable set (SQL: NULL never equi-joins).
        if k.iter().any(|l| matches!(l, Literal::Null)) {
            continue;
        }
        out.push((k, row.clone()));
    }
    Ok(out)
}

fn sort_keyed(v: &mut Keyed) {
    v.sort_by(|a, b| key_ord(&a.0, &b.0).unwrap_or(std::cmp::Ordering::Equal));
}

fn run_end(v: &Keyed, start: usize) -> usize {
    let mut e = start + 1;
    while e < v.len() && key_ord(&v[start].0, &v[e].0) == Some(std::cmp::Ordering::Equal) {
        e += 1;
    }
    e
}

fn extend(left: &[Literal], right: Vec<Literal>) -> Vec<Literal> {
    let mut c = left.to_vec();
    c.extend(right);
    c
}

fn prepend(left: Vec<Literal>, right: &[Literal]) -> Vec<Literal> {
    let mut c = left;
    c.extend_from_slice(right);
    c
}

// ── block nested-loop join ─────────────────────────────────────────────────

/// Block nested-loop join for cross joins (`on == None`) and non-equi
/// conditions. O(n·m) — the optimizer avoids it when a hash/merge join is
/// possible; used only when no equi-key exists.
pub fn nested_loop_join(
    left: Batch,
    right: Batch,
    join_type: JoinType,
    on: &Option<QExpr>,
) -> Result<Batch> {
    let out_schema = combined_schema(&left.schema, &right.schema);
    let right_len = right.schema.len();
    let left_len = left.schema.len();
    let mut out_rows = Vec::new();

    let mut right_matched = vec![false; right.rows.len()];
    for lrow in &left.rows {
        let mut matched = false;
        for (rj, rrow) in right.rows.iter().enumerate() {
            let combined = extend(lrow, rrow.clone());
            let ok = match on {
                None => true,
                Some(cond) => eval_predicate(cond, &out_schema, &combined)?,
            };
            if ok {
                out_rows.push(combined);
                matched = true;
                right_matched[rj] = true;
            }
        }
        if !matched && matches!(join_type, JoinType::Left) {
            out_rows.push(extend(lrow, nulls(right_len)));
        }
    }
    if matches!(join_type, JoinType::Right) {
        for (rj, rrow) in right.rows.iter().enumerate() {
            if !right_matched[rj] {
                out_rows.push(prepend(nulls(left_len), rrow));
            }
        }
    }

    Ok(Batch {
        schema: out_schema,
        rows: out_rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnType;

    fn col(q: &str, n: &str) -> ColumnRef {
        ColumnRef {
            qualifier: q.to_string(),
            name: n.to_string(),
            ty: ColumnType::Int64,
        }
    }

    fn key(q: &str, n: &str) -> QExpr {
        QExpr::Column {
            qualifier: Some(q.to_string()),
            name: n.to_string(),
        }
    }

    fn ints(rows: &[[i64; 2]]) -> Vec<Vec<Literal>> {
        rows.iter()
            .map(|r| vec![Literal::Int(r[0]), Literal::Int(r[1])])
            .collect()
    }

    fn left_batch() -> Batch {
        Batch {
            schema: vec![col("l", "k"), col("l", "v")],
            rows: ints(&[[1, 10], [2, 20], [2, 21], [4, 40]]),
        }
    }

    fn right_batch() -> Batch {
        Batch {
            schema: vec![col("r", "k"), col("r", "w")],
            rows: ints(&[[2, 200], [2, 201], [3, 300], [4, 400]]),
        }
    }

    fn sorted(mut b: Batch) -> Vec<Vec<Literal>> {
        b.rows
            .sort_by(|a, c| format!("{a:?}").cmp(&format!("{c:?}")));
        b.rows
    }

    #[test]
    fn merge_join_inner_matches_hash_join() {
        let lk = vec![key("l", "k")];
        let rk = vec![key("r", "k")];
        let merge = merge_join(
            left_batch(),
            right_batch(),
            JoinType::Inner,
            &lk,
            &rk,
            &None,
        )
        .unwrap();
        let hash = hash_join(
            left_batch(),
            right_batch(),
            JoinType::Inner,
            &lk,
            &rk,
            &None,
            1_000_000,
        )
        .unwrap();
        assert_eq!(sorted(merge), sorted(hash));
    }

    #[test]
    fn merge_join_left_outer_keeps_unmatched_left() {
        let out = merge_join(
            left_batch(),
            right_batch(),
            JoinType::Left,
            &[key("l", "k")],
            &[key("r", "k")],
            &None,
        )
        .unwrap();
        // k=1 unmatched (1) + k=2 cross (2x2=4) + k=4 (1) = 6.
        assert_eq!(out.rows.len(), 6);
        assert!(out
            .rows
            .iter()
            .any(|r| r[0] == Literal::Int(1) && r[2] == Literal::Null));
    }

    #[test]
    fn hash_join_grace_spill_matches_in_memory() {
        let lk = vec![key("l", "k")];
        let rk = vec![key("r", "k")];
        let big_l: Vec<[i64; 2]> = (0..200).map(|i| [i % 20, i]).collect();
        let big_r: Vec<[i64; 2]> = (0..200).map(|i| [i % 20, i * 2]).collect();
        let make = || {
            (
                Batch {
                    schema: vec![col("l", "k"), col("l", "v")],
                    rows: ints(&big_l),
                },
                Batch {
                    schema: vec![col("r", "k"), col("r", "w")],
                    rows: ints(&big_r),
                },
            )
        };
        let (l1, r1) = make();
        let in_mem = hash_join(l1, r1, JoinType::Inner, &lk, &rk, &None, 1_000_000).unwrap();
        let (l2, r2) = make();
        let spilled = hash_join(l2, r2, JoinType::Inner, &lk, &rk, &None, 8).unwrap();
        assert_eq!(sorted(in_mem), sorted(spilled));
    }
}
